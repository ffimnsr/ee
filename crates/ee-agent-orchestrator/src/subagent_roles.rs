//! Built-in subagent role library.
//!
//! Delegation becomes useful and safe when the role decides the tool scope:
//! every built-in role ships a fixed tool-class list and role instructions,
//! and the resulting [`SubagentRole`] plugs into the existing manager and
//! policy machinery unchanged.  Evidence-requiring roles (everything except
//! [`BuiltinSubagentRole::Summarizer`]) expect child summaries to cite the
//! files and tools they claim, enforced by the subagent result verifier.

use serde::{Deserialize, Serialize};

use crate::subagents::{SUBAGENT_DEFAULT_MAX_ITERATIONS, SubagentRole};
use crate::tools::SideEffectClass;

/// One of the built-in subagent roles.
///
/// Roles are tool-scope limited: read-only roles deny writes and executes,
/// the implementer writes only inside assigned scopes, the summarizer gets no
/// tools at all.  Custom roles remain available by constructing
/// [`SubagentRole`] directly.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[non_exhaustive]
pub enum BuiltinSubagentRole {
    /// Read-only research with tool citations.
    Researcher,
    /// Read/search/symbol inspection of code, never writes or executes.
    CodeReader,
    /// Writes only inside the assigned file scopes; no terminal execution.
    Implementer,
    /// Runs the configured validation tools; never writes files.
    TestRunner,
    /// Read-only review plus diagnostics; never writes.
    Reviewer,
    /// Pure summarization from provided context; no tools.
    Summarizer,
}

impl BuiltinSubagentRole {
    /// Every built-in role, in declaration order.
    pub const ALL: [Self; 6] = [
        Self::Researcher,
        Self::CodeReader,
        Self::Implementer,
        Self::TestRunner,
        Self::Reviewer,
        Self::Summarizer,
    ];

    /// Stable role name (also used as the child task title).
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Researcher => "researcher",
            Self::CodeReader => "code_reader",
            Self::Implementer => "implementer",
            Self::TestRunner => "test_runner",
            Self::Reviewer => "reviewer",
            Self::Summarizer => "summarizer",
        }
    }

    /// Role instructions seeded as the child's system message; they tell the
    /// child which tools it may use and to cite evidence as `[file:path]` /
    /// `[tool:name]` markers.
    #[must_use]
    pub fn instructions(self) -> &'static str {
        match self {
            Self::Researcher => {
                "Research the assigned question using read-only tools only. Never modify files, \
                 run commands, or delegate. Cite every file you use as [file:path] and every tool \
                 as [tool:name] in your summary."
            }
            Self::CodeReader => {
                "Inspect and search the assigned code using file, search, and symbol tools only. \
                 Never modify files, run commands, or delegate. Cite every file you inspect as \
                 [file:path] and every tool as [tool:name]."
            }
            Self::Implementer => {
                "Implement the assigned change by writing files only within your assigned scope. \
                 Never run terminal commands. Cite every file you write as [file:path]."
            }
            Self::TestRunner => {
                "Run the configured validation tools and report their results. Never modify files. \
                 Cite every tool you run as [tool:name]."
            }
            Self::Reviewer => {
                "Review the assigned change using read-only and diagnostics tools only. Never \
                 modify files. Cite every file you inspect as [file:path] and every tool as \
                 [tool:name]."
            }
            Self::Summarizer => {
                "Produce a concise summary from the provided context. No tools are available."
            }
        }
    }

    /// Default allowed tool classes for the role.  The implementer's write
    /// access is bounded by the assigned file scopes (role globs) set by the
    /// caller; test_runner and reviewer carry execute-class diagnostics.
    #[must_use]
    pub fn tool_classes(self) -> Vec<SideEffectClass> {
        match self {
            Self::Researcher => vec![SideEffectClass::Read],
            Self::CodeReader => vec![SideEffectClass::Read],
            Self::Implementer => vec![SideEffectClass::Read, SideEffectClass::Write],
            Self::TestRunner => vec![SideEffectClass::Read, SideEffectClass::Execute],
            Self::Reviewer => vec![SideEffectClass::Read, SideEffectClass::Execute],
            Self::Summarizer => Vec::new(),
        }
    }

    /// Whether the role's summaries must cite evidence before their memory
    /// merges into the parent store.  Only the summarizer is exempt; every
    /// other built-in role is evidence-requiring.
    #[must_use]
    pub fn requires_evidence(self) -> bool {
        !matches!(self, Self::Summarizer)
    }

    /// Builds the concrete [`SubagentRole`] for this role with default
    /// iteration cap and no scope globs (callers narrow the implementer's
    /// write scope via [`SubagentRole::with_allowed_scope_globs`]).
    #[must_use]
    pub fn role(self) -> SubagentRole {
        SubagentRole {
            name: self.name().to_string(),
            instructions: self.instructions().to_string(),
            allowed_tool_classes: self.tool_classes(),
            max_iterations: SUBAGENT_DEFAULT_MAX_ITERATIONS,
            allowed_scope_globs: Vec::new(),
            model: None,
        }
    }

    /// Resolves a role name to its built-in; `None` for unknown/custom names.
    #[must_use]
    pub fn by_name(name: &str) -> Option<Self> {
        Self::ALL.iter().copied().find(|role| role.name() == name)
    }
}

/// Fail-closed evidence requirement for an arbitrary role name.
///
/// Built-in roles except the summarizer require citations; unknown/custom
/// role names do not (their callers own the verification decision).
#[must_use]
pub fn requires_evidence_for_name(name: &str) -> bool {
    BuiltinSubagentRole::by_name(name).is_some_and(|role| role.requires_evidence())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn researcher_denies_writes_and_executes() {
        let role = BuiltinSubagentRole::Researcher;
        assert_eq!(role.tool_classes(), vec![SideEffectClass::Read]);
        assert!(!role.tool_classes().contains(&SideEffectClass::Write));
        assert!(!role.tool_classes().contains(&SideEffectClass::Execute));
        assert!(!role.tool_classes().contains(&SideEffectClass::Delegate));
        assert!(role.requires_evidence());
    }

    #[test]
    fn code_reader_denies_writes_and_executes() {
        let role = BuiltinSubagentRole::CodeReader;
        assert_eq!(role.tool_classes(), vec![SideEffectClass::Read]);
        assert!(!role.tool_classes().contains(&SideEffectClass::Write));
        assert!(!role.tool_classes().contains(&SideEffectClass::Execute));
        assert!(role.requires_evidence());
    }

    #[test]
    fn implementer_writes_only_in_assigned_scopes_without_terminal() {
        let role = BuiltinSubagentRole::Implementer;
        assert!(role.tool_classes().contains(&SideEffectClass::Read));
        assert!(role.tool_classes().contains(&SideEffectClass::Write));
        assert!(
            !role.tool_classes().contains(&SideEffectClass::Execute),
            "implementer denies terminal execution by default"
        );
        assert!(role.requires_evidence());
    }

    #[test]
    fn test_runner_runs_validation_tools_without_writing_files() {
        let role = BuiltinSubagentRole::TestRunner;
        assert!(role.tool_classes().contains(&SideEffectClass::Execute));
        assert!(
            !role.tool_classes().contains(&SideEffectClass::Write),
            "test_runner denies file writes"
        );
        assert!(role.requires_evidence());
    }

    #[test]
    fn reviewer_uses_read_only_and_diagnostics_without_writing() {
        let role = BuiltinSubagentRole::Reviewer;
        assert!(role.tool_classes().contains(&SideEffectClass::Read));
        assert!(role.tool_classes().contains(&SideEffectClass::Execute));
        assert!(!role.tool_classes().contains(&SideEffectClass::Write), "reviewer denies writes");
        assert!(role.requires_evidence());
    }

    #[test]
    fn summarizer_denies_all_tools_by_default() {
        let role = BuiltinSubagentRole::Summarizer;
        assert!(role.tool_classes().is_empty(), "summarizer gets no tools");
        assert!(!role.requires_evidence(), "summarizer output needs no citations");
    }

    #[test]
    fn role_builder_produces_matching_subagent_role() {
        for builtin in BuiltinSubagentRole::ALL {
            let role = builtin.role();
            assert_eq!(role.name, builtin.name());
            assert_eq!(role.instructions, builtin.instructions());
            assert_eq!(role.allowed_tool_classes, builtin.tool_classes());
            assert_eq!(role.max_iterations, SUBAGENT_DEFAULT_MAX_ITERATIONS);
            assert!(role.allowed_scope_globs.is_empty(), "scopes are caller-assigned");
        }
    }

    #[test]
    fn requires_evidence_mapping_is_fail_closed_for_builtins() {
        for builtin in BuiltinSubagentRole::ALL {
            assert_eq!(
                requires_evidence_for_name(builtin.name()),
                builtin.requires_evidence(),
                "name lookup must agree with the enum"
            );
        }
        assert!(!requires_evidence_for_name("worker"), "custom roles opt in");
        assert!(!requires_evidence_for_name(""), "unknown names do not require evidence");
    }

    #[test]
    fn by_name_resolves_every_role_and_rejects_unknowns() {
        for builtin in BuiltinSubagentRole::ALL {
            assert_eq!(BuiltinSubagentRole::by_name(builtin.name()), Some(builtin));
        }
        assert_eq!(BuiltinSubagentRole::by_name("robot"), None);
        assert_eq!(BuiltinSubagentRole::by_name(""), None);
    }

    #[test]
    fn builtin_roles_roundtrip_through_json() {
        for builtin in BuiltinSubagentRole::ALL {
            let json = serde_json::to_string(&builtin).expect("serializes");
            let restored: BuiltinSubagentRole = serde_json::from_str(&json).expect("parses");
            assert_eq!(restored, builtin);
        }
    }
}
