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

#[derive(Serialize, Deserialize)]
/// Language Specific Configuration
pub struct LanguageConfig {
    pub language_name: String,
    pub start_command: String,
    pub start_arguments: Vec<String>,
    pub extensions: Vec<String>,
    pub supports_single_file: bool,
    pub workspace_identifier: Option<String>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    #[serde(default)]
    pub initialization_options: Option<Value>,
}

#[derive(Serialize, Deserialize)]
pub struct DisabledLanguageConfig {
    pub extensions: Vec<String>,
}

/// Represents the config for the Language Plugin
#[derive(Serialize, Deserialize)]
pub struct Config {
    pub language_config: HashMap<String, LanguageConfig>,
    #[serde(default)]
    pub disabled_language_config: HashMap<String, DisabledLanguageConfig>,
}

impl Config {
    pub fn bundled() -> Self {
        Self {
            language_config: HashMap::from([
                (
                    "rust".to_owned(),
                    LanguageConfig {
                        language_name: "Rust".to_owned(),
                        start_command: "rls".to_owned(),
                        start_arguments: Vec::new(),
                        extensions: vec!["rs".to_owned()],
                        supports_single_file: false,
                        workspace_identifier: Some("Cargo.toml".to_owned()),
                        env: BTreeMap::new(),
                        initialization_options: None,
                    },
                ),
                (
                    "json".to_owned(),
                    LanguageConfig {
                        language_name: "Json".to_owned(),
                        start_command: "vscode-json-languageserver".to_owned(),
                        start_arguments: vec!["--stdio".to_owned()],
                        extensions: vec!["json".to_owned(), "jsonc".to_owned()],
                        supports_single_file: true,
                        workspace_identifier: None,
                        env: BTreeMap::new(),
                        initialization_options: None,
                    },
                ),
                (
                    "yaml".to_owned(),
                    LanguageConfig {
                        language_name: "Yaml".to_owned(),
                        start_command: "yaml-language-server".to_owned(),
                        start_arguments: vec!["--stdio".to_owned()],
                        extensions: vec!["yaml".to_owned(), "yml".to_owned()],
                        supports_single_file: true,
                        workspace_identifier: None,
                        env: BTreeMap::new(),
                        initialization_options: None,
                    },
                ),
                (
                    "typescript".to_owned(),
                    LanguageConfig {
                        language_name: "Typescript".to_owned(),
                        start_command: "javascript-typescript-stdio".to_owned(),
                        start_arguments: Vec::new(),
                        extensions: vec![
                            "ts".to_owned(),
                            "js".to_owned(),
                            "jsx".to_owned(),
                            "tsx".to_owned(),
                        ],
                        supports_single_file: true,
                        workspace_identifier: Some("package.json".to_owned()),
                        env: BTreeMap::new(),
                        initialization_options: None,
                    },
                ),
            ]),
            disabled_language_config: HashMap::new(),
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

        let rust = config.language_config.get("rust").unwrap();
        assert_eq!(rust.language_name, "Rust");
        assert_eq!(rust.start_command, "rls");
        assert!(rust.start_arguments.is_empty());
        assert_eq!(rust.extensions, vec!["rs"]);
        assert!(!rust.supports_single_file);
        assert_eq!(rust.workspace_identifier.as_deref(), Some("Cargo.toml"));
        assert!(rust.env.is_empty());
        assert_eq!(rust.initialization_options, None);

        let json = config.language_config.get("json").unwrap();
        assert_eq!(json.language_name, "Json");
        assert_eq!(json.start_command, "vscode-json-languageserver");
        assert_eq!(json.start_arguments, vec!["--stdio"]);
        assert_eq!(json.extensions, vec!["json", "jsonc"]);
        assert!(json.supports_single_file);
        assert_eq!(json.workspace_identifier, None);
        assert!(json.env.is_empty());
        assert_eq!(json.initialization_options, None);

        let yaml = config.language_config.get("yaml").unwrap();
        assert_eq!(yaml.language_name, "Yaml");
        assert_eq!(yaml.start_command, "yaml-language-server");
        assert_eq!(yaml.start_arguments, vec!["--stdio"]);
        assert_eq!(yaml.extensions, vec!["yaml", "yml"]);
        assert!(yaml.supports_single_file);
        assert_eq!(yaml.workspace_identifier, None);
        assert!(yaml.env.is_empty());
        assert_eq!(yaml.initialization_options, None);

        let typescript = config.language_config.get("typescript").unwrap();
        assert_eq!(typescript.language_name, "Typescript");
        assert_eq!(typescript.start_command, "javascript-typescript-stdio");
        assert!(typescript.start_arguments.is_empty());
        assert_eq!(typescript.extensions, vec!["ts", "js", "jsx", "tsx"]);
        assert!(typescript.supports_single_file);
        assert_eq!(typescript.workspace_identifier.as_deref(), Some("package.json"));
        assert!(typescript.env.is_empty());
        assert_eq!(typescript.initialization_options, None);
    }
}
