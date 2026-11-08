//! Stable error and diagnostic classes.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::contribution::ContributionError;
use crate::event::EventError;
use crate::ids::IdError;
use crate::manifest::ManifestError;
use crate::service::ServiceError;

/// Stable diagnostic class reported by the host and `ee do plugins` commands.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ErrorClass {
    Manifest,
    Schema,
    Dependency,
    Permission,
    Transport,
    Contribution,
    Config,
    Resource,
    Runtime,
    Io,
}

impl ErrorClass {
    /// Stable machine-readable class name.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Manifest => "manifest",
            Self::Schema => "schema",
            Self::Dependency => "dependency",
            Self::Permission => "permission",
            Self::Transport => "transport",
            Self::Contribution => "contribution",
            Self::Config => "config",
            Self::Resource => "resource",
            Self::Runtime => "runtime",
            Self::Io => "io",
        }
    }
}

/// Error returned by plugin contracts and hosts.
#[derive(Debug, thiserror::Error)]
pub enum PluginError {
    #[error("manifest error: {0}")]
    Manifest(ManifestError),
    #[error("schema error: {message}")]
    Schema { message: String },
    #[error("dependency error: {message}")]
    Dependency { message: String },
    #[error("permission denied: {permission}")]
    Permission { permission: String },
    #[error("transport error: {message}")]
    Transport { message: String },
    #[error("contribution error: {0}")]
    Contribution(ContributionError),
    #[error("config error: {message}")]
    Config { message: String },
    #[error("resource limit exceeded: {message}")]
    Resource { message: String },
    #[error("runtime error: {message}")]
    Runtime { message: String },
    #[error("io error: {message}")]
    Io { message: String },
    #[error("service error: {0}")]
    Service(ServiceError),
    #[error("event error: {0}")]
    Event(EventError),
    #[error("invalid identifier: {0}")]
    Id(IdError),
    #[error("operation is not supported by this host: {feature}")]
    Unsupported { feature: &'static str },
    #[error("operation cancelled")]
    Cancelled,
}

impl From<ManifestError> for PluginError {
    fn from(value: ManifestError) -> Self {
        Self::Manifest(value)
    }
}

impl From<ContributionError> for PluginError {
    fn from(value: ContributionError) -> Self {
        Self::Contribution(value)
    }
}

impl From<ServiceError> for PluginError {
    fn from(value: ServiceError) -> Self {
        Self::Service(value)
    }
}

impl From<EventError> for PluginError {
    fn from(value: EventError) -> Self {
        Self::Event(value)
    }
}

impl From<IdError> for PluginError {
    fn from(value: IdError) -> Self {
        Self::Id(value)
    }
}

impl From<std::io::Error> for PluginError {
    fn from(value: std::io::Error) -> Self {
        Self::Io { message: value.to_string() }
    }
}

impl PluginError {
    /// Stable diagnostic class.
    pub fn class(&self) -> ErrorClass {
        match self {
            Self::Manifest(_) => ErrorClass::Manifest,
            Self::Schema { .. } => ErrorClass::Schema,
            Self::Dependency { .. } => ErrorClass::Dependency,
            Self::Permission { .. } => ErrorClass::Permission,
            Self::Transport { .. } => ErrorClass::Transport,
            Self::Contribution(_) => ErrorClass::Contribution,
            Self::Config { .. } => ErrorClass::Config,
            Self::Resource { .. } => ErrorClass::Resource,
            Self::Runtime { .. } | Self::Unsupported { .. } | Self::Cancelled => {
                ErrorClass::Runtime
            }
            Self::Io { .. } => ErrorClass::Io,
            Self::Service(ServiceError::Missing { .. } | ServiceError::TypeMismatch { .. }) => {
                ErrorClass::Dependency
            }
            Self::Service(ServiceError::Duplicate { .. }) => ErrorClass::Dependency,
            Self::Event(EventError::ListenerLimit { .. } | EventError::DispatchDepth { .. }) => {
                ErrorClass::Resource
            }
            Self::Event(EventError::Cancelled { .. }) => ErrorClass::Runtime,
            Self::Event(EventError::BailTypeMismatch { .. }) => ErrorClass::Contribution,
            Self::Id(_) => ErrorClass::Manifest,
        }
    }

    /// Stable code for logs and tests.
    pub fn code(&self) -> &'static str {
        match self {
            Self::Manifest(_) => "plugin.manifest",
            Self::Schema { .. } => "plugin.schema",
            Self::Dependency { .. } => "plugin.dependency",
            Self::Permission { .. } => "plugin.permission",
            Self::Transport { .. } => "plugin.transport",
            Self::Contribution(_) => "plugin.contribution",
            Self::Config { .. } => "plugin.config",
            Self::Resource { .. } => "plugin.resource",
            Self::Runtime { .. } => "plugin.runtime",
            Self::Io { .. } => "plugin.io",
            Self::Service(_) => "plugin.service",
            Self::Event(_) => "plugin.event",
            Self::Id(_) => "plugin.id",
            Self::Unsupported { .. } => "plugin.unsupported",
            Self::Cancelled => "plugin.cancelled",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classes_are_assigned_deterministically() {
        assert_eq!(PluginError::Cancelled.class(), ErrorClass::Runtime);
        assert_eq!(
            PluginError::Permission { permission: "editor.fs.write".to_owned() }.class(),
            ErrorClass::Permission
        );
        assert_eq!(
            PluginError::Service(ServiceError::Missing {
                id: crate::ids::ServiceId::from_static("editor.document@1"),
            })
            .class(),
            ErrorClass::Dependency
        );
    }

    #[test]
    fn io_errors_convert() {
        let error: PluginError = std::io::Error::other("boom").into();
        assert_eq!(error.class(), ErrorClass::Io);
    }
}
