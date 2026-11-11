//! Typed prompt assembly errors.
//!
//! Every failure is explicit: strict interpolation and section validation are
//! allowed to fail assembly, never to emit placeholder text into a model
//! request.

use std::fmt;

/// Prompt assembly failure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum PromptError {
    /// A section was registered with an empty identity.
    EmptyRegistration,
    /// Two sections in the same plane share a registration name.
    DuplicateRegistration {
        /// The duplicated registration name.
        registration: String,
    },
    /// More than one effective section claimed `complete`.
    MultipleCompleteSections {
        /// Number of effective complete sections.
        count: usize,
    },
    /// A `{{variable}}` was not supplied by the caller.
    UnknownVariable {
        /// Section that referenced the variable.
        section: String,
        /// Variable name.
        variable: String,
    },
    /// A `{{variable}}` was supplied with an empty value.
    UndefinedVariable {
        /// Section that referenced the variable.
        section: String,
        /// Variable name.
        variable: String,
    },
    /// A `{{` opener had no valid closing delimiter or name.
    MalformedVariable {
        /// Section that contained the malformed expression.
        section: String,
    },
    /// Rendered output exceeded the configured byte budget.
    ByteBudgetExceeded {
        /// Rendered bytes.
        used: usize,
        /// Configured ceiling.
        max: usize,
    },
}

impl fmt::Display for PromptError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptyRegistration => formatter.write_str("prompt section registration is empty"),
            Self::DuplicateRegistration { registration } => {
                write!(formatter, "duplicate prompt section registration: {registration}")
            }
            Self::MultipleCompleteSections { count } => {
                write!(
                    formatter,
                    "{count} prompt sections claimed complete; at most one is allowed"
                )
            }
            Self::UnknownVariable { section, variable } => {
                write!(formatter, "prompt section {section} references unknown variable {variable}")
            }
            Self::UndefinedVariable { section, variable } => {
                write!(
                    formatter,
                    "prompt section {section} references undefined variable {variable}"
                )
            }
            Self::MalformedVariable { section } => {
                write!(
                    formatter,
                    "prompt section {section} contains a malformed variable expression"
                )
            }
            Self::ByteBudgetExceeded { used, max } => {
                write!(formatter, "prompt is {used} bytes, over the {max}-byte budget")
            }
        }
    }
}

impl std::error::Error for PromptError {}
