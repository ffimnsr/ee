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

use std::collections::HashMap;
use std::fmt;
use std::io::Error as IOError;

use jsonrpc_lite::Error as JsonRpcError;
use lsp_types::{Command, TextEdit};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use xi_core_lib::plugin_rpc::{CompletionSuggestion, NavigationTarget};
use xi_plugin_lib::Diagnostic as CoreDiagnostic;
use xi_plugin_lib::Error as PluginLibError;
use xi_rpc::RemoteError;

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
}

/// Represents the config for the Language Plugin
#[derive(Serialize, Deserialize)]
pub struct Config {
    pub language_config: HashMap<String, LanguageConfig>,
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

impl From<LanguageResponseError> for RemoteError {
    fn from(src: LanguageResponseError) -> Self {
        match src {
            LanguageResponseError::NullResponse => {
                RemoteError::custom(0, "null response from server", None)
            }
            LanguageResponseError::FallbackResponse => {
                RemoteError::custom(1, "fallback response from server", None)
            }
            LanguageResponseError::LanguageServerError(error) => {
                RemoteError::custom(2, "language server error occured", Some(Value::String(error)))
            }
            LanguageResponseError::PluginLibError(error) => RemoteError::custom(
                3,
                "Plugin Lib Error",
                Some(Value::String(format!("{:?}", error))),
            ),
            LanguageResponseError::Transport(error) => RemoteError::custom(
                4,
                "language server transport error",
                Some(Value::String(error)),
            ),
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

#[derive(Debug)]
pub enum LspResponse {
    Hover(Result<Hover, LanguageResponseError>),
    Diagnostics(Result<Vec<CoreDiagnostic>, LanguageResponseError>),
    Completions(Result<Vec<CompletionSuggestion>, LanguageResponseError>),
    Locations { title: String, result: Result<Vec<NavigationTarget>, LanguageResponseError> },
    Formatting { title: String, result: Result<Vec<TextEdit>, LanguageResponseError> },
    CodeActions(Result<Vec<LspCodeAction>, LanguageResponseError>),
}
