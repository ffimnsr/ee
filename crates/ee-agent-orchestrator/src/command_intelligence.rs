//! Declared validation-command metadata and typed execution classifications.
//!
//! This module keeps command selection metadata separate from shell execution.
//! Validation commands still run only through [`crate::tools::ToolExecutor`],
//! preserving its argument validation, workspace scope, policy, host approval,
//! cancellation, timeout, and output-boundary gates.

use serde::{Deserialize, Serialize};

/// Stable schema version for workspace-declared validation command metadata.
pub const VALIDATION_COMMAND_SCHEMA_VERSION: u32 = 1;

/// Scope of validation work declared by a workspace.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ValidationScope {
    /// Smallest check associated with changed files or resolved symbols.
    #[default]
    Targeted,
    /// Broader package or workspace check. It runs only after focused checks
    /// pass unless changed scope has no focused command.
    Workspace,
}

/// Approval routing declared for a validation command.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ValidationApprovalClass {
    /// Normal policy routing. Every command still passes the shared policy gate.
    #[default]
    Policy,
    /// A trusted host must approve this command. A command definition with
    /// `host_approval` performs that prompt; otherwise policy denies by default.
    Host,
}

/// Why a plan entry is eligible to run now.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ValidationEscalation {
    /// Directly selected from changed scope or no focused candidate exists.
    #[default]
    Direct,
    /// Broader check selected only after all earlier focused commands pass.
    AfterFocusedPass,
}

/// Stable, workspace-declared metadata for one validation command.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct ValidationCommandMetadata {
    /// Stable workspace command identifier. This is evidence identity, not a
    /// shell string, and must remain stable across command wording changes.
    pub command_id: String,
    /// Validation breadth used for focused-first selection and escalation.
    #[serde(default)]
    pub scope: ValidationScope,
    /// Command ids that must have passed earlier in the same plan.
    #[serde(default)]
    pub prerequisites: Vec<String>,
    /// Approval route required in addition to shared command policy.
    #[serde(default)]
    pub approval: ValidationApprovalClass,
    /// Stable test/check ids affected by this command.
    #[serde(default)]
    pub test_ids: Vec<String>,
}

impl ValidationCommandMetadata {
    /// Builds targeted metadata using `command_id` as its stable identity.
    #[must_use]
    pub fn targeted(command_id: impl Into<String>) -> Self {
        Self {
            command_id: command_id.into(),
            scope: ValidationScope::Targeted,
            prerequisites: Vec::new(),
            approval: ValidationApprovalClass::Policy,
            test_ids: Vec::new(),
        }
    }

    /// Changes validation breadth.
    #[must_use]
    pub fn with_scope(mut self, scope: ValidationScope) -> Self {
        self.scope = scope;
        self
    }

    /// Adds prerequisite command ids.
    #[must_use]
    pub fn with_prerequisites(
        mut self,
        prerequisites: impl IntoIterator<Item = impl Into<String>>,
    ) -> Self {
        self.prerequisites = prerequisites.into_iter().map(Into::into).collect();
        self
    }

    /// Sets approval routing.
    #[must_use]
    pub fn with_approval(mut self, approval: ValidationApprovalClass) -> Self {
        self.approval = approval;
        self
    }

    /// Adds stable affected test/check ids.
    #[must_use]
    pub fn with_test_ids(mut self, test_ids: impl IntoIterator<Item = impl Into<String>>) -> Self {
        self.test_ids = test_ids.into_iter().map(Into::into).collect();
        self
    }

    /// Returns false for metadata that cannot safely identify a command.
    #[must_use]
    pub fn is_valid(&self) -> bool {
        !self.command_id.trim().is_empty()
            && !self.prerequisites.iter().any(|id| id.trim().is_empty() || id == &self.command_id)
    }
}

/// Typed terminal classification for a validation command result.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ValidationCommandFailure {
    /// Command ran and returned a non-zero or tool-level failure.
    CommandFailed,
    /// Command or executor reached its timeout.
    Timeout,
    /// Command was cancelled by the turn or host.
    Cancelled,
    /// Shared policy or required approval denied dispatch.
    PolicyDenied,
    /// Declared prerequisite command did not pass or is absent from the plan.
    MissingDependency,
    /// Required tool/capability/environment is unavailable before execution.
    UnavailableEnvironment,
    /// Tool arguments were invalid and dispatch did not occur.
    InvalidArguments,
}

impl ValidationCommandFailure {
    /// Stable spelling for protocol, trace, and final-response evidence.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::CommandFailed => "command_failed",
            Self::Timeout => "timeout",
            Self::Cancelled => "cancelled",
            Self::PolicyDenied => "policy_denied",
            Self::MissingDependency => "missing_dependency",
            Self::UnavailableEnvironment => "unavailable_environment",
            Self::InvalidArguments => "invalid_arguments",
        }
    }
}
