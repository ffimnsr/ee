//! Harness-loop errors.

use std::fmt;

/// Result alias for the harnessing crate.
pub type HarnessResult<T> = Result<T, HarnessError>;

/// Typed harness-loop failure.
#[derive(Debug)]
pub enum HarnessError {
    /// A hard budget cap or ledger identity failed.
    Budget(String),
    /// A candidate tree failed validation twice and could not be inherited.
    Validation(String),
    /// The injected channel failed.
    Channel(String),
    /// Filesystem read/write failed.
    Io(String),
    /// A model reply could not be parsed under its contract.
    Parse(String),
    /// Run state, resume, or cache invariants failed.
    State(String),
}

impl fmt::Display for HarnessError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Budget(message) => write!(formatter, "harness budget: {message}"),
            Self::Validation(message) => write!(formatter, "harness validation: {message}"),
            Self::Channel(message) => write!(formatter, "harness channel: {message}"),
            Self::Io(message) => write!(formatter, "harness io: {message}"),
            Self::Parse(message) => write!(formatter, "harness parse: {message}"),
            Self::State(message) => write!(formatter, "harness state: {message}"),
        }
    }
}

impl std::error::Error for HarnessError {}

impl From<std::io::Error> for HarnessError {
    fn from(error: std::io::Error) -> Self {
        Self::Io(error.to_string())
    }
}

impl From<String> for HarnessError {
    fn from(message: String) -> Self {
        Self::State(message)
    }
}
