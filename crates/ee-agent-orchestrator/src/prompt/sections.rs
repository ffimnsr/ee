//! DSH-compatible prompt section registry.
//!
//! Slot names and numeric orders mirror DeepSeek Harness `SECTION_ORDERS` and
//! `CONTEXT_ORDERS` exactly, including slots that `ee` cannot emit yet.
//! Registration names are the identity layer used for duplicate detection and
//! future scoped overrides; slot names only order sections.

/// One registered prompt slot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Slot {
    /// Stable slot name.
    pub name: &'static str,
    /// Numeric order copied from DSH.
    pub order: i32,
    /// Registration names that may contribute to this slot.
    pub registrations: &'static [&'static str],
}

impl Slot {
    /// Registration name used when a contributor does not supply its own.
    #[must_use]
    pub(crate) fn default_registration(self) -> &'static str {
        self.registrations.first().copied().unwrap_or_default()
    }
}

// Stable system-prompt slots, ordered exactly like DSH.
pub(crate) const HARNESS_IDENTITY: Slot =
    Slot { name: "HARNESS_IDENTITY", order: -1000, registrations: &["harness:identity"] };
pub(crate) const DEPLOYMENT_PERSONA_PREFIX: Slot = Slot {
    name: "DEPLOYMENT_PERSONA_PREFIX",
    order: 0,
    registrations: &["deployment:persona-prefix"],
};
pub(crate) const PLAN_POLICY: Slot =
    Slot { name: "PLAN_POLICY", order: 500, registrations: &["plan:policy"] };
pub(crate) const TEAM_POLICY: Slot =
    Slot { name: "TEAM_POLICY", order: 600, registrations: &["team:policy"] };
pub(crate) const PTC_ONLY: Slot =
    Slot { name: "PTC_ONLY", order: 800, registrations: &["tools:ptc-only"] };
pub(crate) const FILE_REFERENCE: Slot =
    Slot { name: "FILE_REFERENCE", order: 900, registrations: &["context:file-reference"] };
pub(crate) const TOOL_BASH: Slot =
    Slot { name: "TOOL_BASH", order: 1000, registrations: &["tool:bash"] };
pub(crate) const TOOL_PWSH: Slot =
    Slot { name: "TOOL_PWSH", order: 1010, registrations: &["tool:pwsh"] };
pub(crate) const TOOL_READ: Slot =
    Slot { name: "TOOL_READ", order: 1100, registrations: &["tool:read"] };
pub(crate) const TOOL_WRITE: Slot =
    Slot { name: "TOOL_WRITE", order: 1200, registrations: &["tool:write"] };
pub(crate) const TOOL_EDIT: Slot =
    Slot { name: "TOOL_EDIT", order: 1300, registrations: &["tool:edit"] };
pub(crate) const TOOL_APPLY_PATCH: Slot =
    Slot { name: "TOOL_APPLY_PATCH", order: 1350, registrations: &["tool:apply_patch"] };
pub(crate) const TOOL_GLOB: Slot =
    Slot { name: "TOOL_GLOB", order: 1400, registrations: &["tool:glob"] };
pub(crate) const TOOL_GREP: Slot =
    Slot { name: "TOOL_GREP", order: 1500, registrations: &["tool:grep"] };
pub(crate) const TOOL_JOBS: Slot =
    Slot { name: "TOOL_JOBS", order: 1600, registrations: &["tool:jobs"] };
pub(crate) const TOOL_PTY: Slot =
    Slot { name: "TOOL_PTY", order: 1700, registrations: &["tool:pty"] };
pub(crate) const TOOL_WEB_SEARCH: Slot =
    Slot { name: "TOOL_WEB_SEARCH", order: 2000, registrations: &["tool:web_search"] };
pub(crate) const TOOL_WEB_FETCH: Slot =
    Slot { name: "TOOL_WEB_FETCH", order: 2100, registrations: &["tool:web_fetch"] };
pub(crate) const TOOL_LSP: Slot =
    Slot { name: "TOOL_LSP", order: 2200, registrations: &["tool:lsp"] };
pub(crate) const TOOL_SESSION_QUERY: Slot =
    Slot { name: "TOOL_SESSION_QUERY", order: 2300, registrations: &["tool:session-query"] };
pub(crate) const TOOL_GOAL: Slot =
    Slot { name: "TOOL_GOAL", order: 2400, registrations: &["tool:goal"] };
pub(crate) const TOOL_SCHEDULE: Slot =
    Slot { name: "TOOL_SCHEDULE", order: 2500, registrations: &["tool:schedule"] };
pub(crate) const TOOL_WORKFLOW: Slot =
    Slot { name: "TOOL_WORKFLOW", order: 2600, registrations: &["tool:workflow"] };
pub(crate) const TOOL_RALPH: Slot =
    Slot { name: "TOOL_RALPH", order: 2700, registrations: &["tool:ralph"] };
pub(crate) const TOOL_SUBAGENT: Slot =
    Slot { name: "TOOL_SUBAGENT", order: 2800, registrations: &["tool:<subagent-tool>"] };
pub(crate) const TOOL_REPORT: Slot = Slot { name: "TOOL_REPORT", order: 2900, registrations: &[] };
pub(crate) const TOOL_COMPUTER_USE: Slot = Slot {
    name: "TOOL_COMPUTER_USE",
    order: 3000,
    registrations: &["browser-use:*", "computer-use:*"],
};
pub(crate) const MCP_SERVERS: Slot = Slot {
    name: "MCP_SERVERS",
    order: 3100,
    registrations: &["mcp:<server>", "mcp-resource-servers"],
};
pub(crate) const TOOLS_SDK: Slot =
    Slot { name: "TOOLS_SDK", order: 5000, registrations: &["tools:sdk"] };
pub(crate) const DELIVERABLE_FILE_REFERENCES: Slot = Slot {
    name: "DELIVERABLE_FILE_REFERENCES",
    order: 9000,
    registrations: &["ui:deliverable-file-references"],
};
pub(crate) const STRUCTURED_OUTPUT: Slot =
    Slot { name: "STRUCTURED_OUTPUT", order: 9900, registrations: &["tool:structured_output"] };
pub(crate) const HARNESS_SOURCE: Slot =
    Slot { name: "HARNESS_SOURCE", order: 10000, registrations: &["harness:source"] };
pub(crate) const WEB_SURFACE: Slot =
    Slot { name: "WEB_SURFACE", order: 10100, registrations: &["app:web-surface"] };
pub(crate) const DEPLOYMENT_PERSONA_SUFFIX: Slot = Slot {
    name: "DEPLOYMENT_PERSONA_SUFFIX",
    order: 10200,
    registrations: &["deployment:persona-suffix"],
};

/// Every stable system-prompt slot, ascending by order.
pub(crate) const STABLE_SECTIONS: &[Slot] = &[
    HARNESS_IDENTITY,
    DEPLOYMENT_PERSONA_PREFIX,
    PLAN_POLICY,
    TEAM_POLICY,
    PTC_ONLY,
    FILE_REFERENCE,
    TOOL_BASH,
    TOOL_PWSH,
    TOOL_READ,
    TOOL_WRITE,
    TOOL_EDIT,
    TOOL_APPLY_PATCH,
    TOOL_GLOB,
    TOOL_GREP,
    TOOL_JOBS,
    TOOL_PTY,
    TOOL_WEB_SEARCH,
    TOOL_WEB_FETCH,
    TOOL_LSP,
    TOOL_SESSION_QUERY,
    TOOL_GOAL,
    TOOL_SCHEDULE,
    TOOL_WORKFLOW,
    TOOL_RALPH,
    TOOL_SUBAGENT,
    TOOL_REPORT,
    TOOL_COMPUTER_USE,
    MCP_SERVERS,
    TOOLS_SDK,
    DELIVERABLE_FILE_REFERENCES,
    STRUCTURED_OUTPUT,
    HARNESS_SOURCE,
    WEB_SURFACE,
    DEPLOYMENT_PERSONA_SUFFIX,
];

// Runtime-context slots. `EE_TURN_GUIDANCE` and `MCP_SERVER_INSTRUCTIONS` are
// additive `ee` extensions; DSH has no equivalent.
pub(crate) const WORKING_DIRECTORY: Slot =
    Slot { name: "WORKING_DIRECTORY", order: 100, registrations: &["working-directory:current"] };
pub(crate) const SANDBOX_POLICY: Slot =
    Slot { name: "SANDBOX_POLICY", order: 110, registrations: &["sandbox:policy"] };
pub(crate) const APPROVAL_POLICY: Slot =
    Slot { name: "APPROVAL_POLICY", order: 115, registrations: &["approval:policy"] };
pub(crate) const SUBAGENT_DELEGATION: Slot = Slot {
    name: "SUBAGENT_DELEGATION",
    order: 120,
    registrations: &["subagent:identity", "subagent:delegation", "tool:<name>:depth"],
};
pub(crate) const RUNTIME_HARNESS_SOURCE: Slot =
    Slot { name: "HARNESS_SOURCE", order: 200, registrations: &["harness:source"] };
pub(crate) const RUNTIME_WEB_SURFACE: Slot =
    Slot { name: "WEB_SURFACE", order: 210, registrations: &["app:web-surface"] };
pub(crate) const EE_TURN_GUIDANCE: Slot =
    Slot { name: "EE_TURN_GUIDANCE", order: 130, registrations: &["ee:turn-guidance"] };
pub(crate) const MCP_SERVER_INSTRUCTIONS: Slot =
    Slot { name: "MCP_SERVER_INSTRUCTIONS", order: 140, registrations: &["mcp:<server>"] };

/// Every runtime-context slot, ascending by order.
pub(crate) const CONTEXT_SECTIONS: &[Slot] = &[
    WORKING_DIRECTORY,
    SANDBOX_POLICY,
    APPROVAL_POLICY,
    SUBAGENT_DELEGATION,
    EE_TURN_GUIDANCE,
    MCP_SERVER_INSTRUCTIONS,
    RUNTIME_HARNESS_SOURCE,
    RUNTIME_WEB_SURFACE,
];

/// Looks up a stable slot by exact slot name.
#[must_use]
pub(crate) fn stable_slot(name: &str) -> Option<&'static Slot> {
    STABLE_SECTIONS.iter().find(|slot| slot.name == name)
}

/// Looks up a runtime-context slot by exact slot name.
#[must_use]
pub(crate) fn runtime_slot(name: &str) -> Option<&'static Slot> {
    CONTEXT_SECTIONS.iter().find(|slot| slot.name == name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stable_orders_match_dsh() {
        let expected = [
            ("HARNESS_IDENTITY", -1000),
            ("DEPLOYMENT_PERSONA_PREFIX", 0),
            ("PLAN_POLICY", 500),
            ("TEAM_POLICY", 600),
            ("PTC_ONLY", 800),
            ("FILE_REFERENCE", 900),
            ("TOOL_BASH", 1000),
            ("TOOL_PWSH", 1010),
            ("TOOL_READ", 1100),
            ("TOOL_WRITE", 1200),
            ("TOOL_EDIT", 1300),
            ("TOOL_APPLY_PATCH", 1350),
            ("TOOL_GLOB", 1400),
            ("TOOL_GREP", 1500),
            ("TOOL_JOBS", 1600),
            ("TOOL_PTY", 1700),
            ("TOOL_WEB_SEARCH", 2000),
            ("TOOL_WEB_FETCH", 2100),
            ("TOOL_LSP", 2200),
            ("TOOL_SESSION_QUERY", 2300),
            ("TOOL_GOAL", 2400),
            ("TOOL_SCHEDULE", 2500),
            ("TOOL_WORKFLOW", 2600),
            ("TOOL_RALPH", 2700),
            ("TOOL_SUBAGENT", 2800),
            ("TOOL_REPORT", 2900),
            ("TOOL_COMPUTER_USE", 3000),
            ("MCP_SERVERS", 3100),
            ("TOOLS_SDK", 5000),
            ("DELIVERABLE_FILE_REFERENCES", 9000),
            ("STRUCTURED_OUTPUT", 9900),
            ("HARNESS_SOURCE", 10000),
            ("WEB_SURFACE", 10100),
            ("DEPLOYMENT_PERSONA_SUFFIX", 10200),
        ];
        let actual: Vec<_> = STABLE_SECTIONS.iter().map(|slot| (slot.name, slot.order)).collect();
        assert_eq!(actual, expected);
    }

    #[test]
    fn runtime_orders_match_dsh_plus_ee_extensions() {
        let expected = [
            ("WORKING_DIRECTORY", 100),
            ("SANDBOX_POLICY", 110),
            ("APPROVAL_POLICY", 115),
            ("SUBAGENT_DELEGATION", 120),
            ("EE_TURN_GUIDANCE", 130),
            ("MCP_SERVER_INSTRUCTIONS", 140),
            ("HARNESS_SOURCE", 200),
            ("WEB_SURFACE", 210),
        ];
        let actual: Vec<_> = CONTEXT_SECTIONS.iter().map(|slot| (slot.name, slot.order)).collect();
        assert_eq!(actual, expected);
    }

    #[test]
    fn registrations_are_unique_within_each_plane() {
        for sections in [STABLE_SECTIONS, CONTEXT_SECTIONS] {
            let mut names = std::collections::BTreeSet::new();
            for slot in sections {
                for registration in slot.registrations {
                    assert!(!registration.is_empty(), "{} has an empty registration", slot.name);
                    assert!(
                        names.insert(*registration),
                        "duplicate registration {registration} in one plane"
                    );
                }
            }
        }
    }

    #[test]
    fn lookups_are_exact() {
        assert_eq!(stable_slot("TOOL_READ").map(|slot| slot.order), Some(1100));
        assert_eq!(runtime_slot("WORKING_DIRECTORY").map(|slot| slot.order), Some(100));
        assert!(stable_slot("tool:read").is_none());
        assert!(runtime_slot("TOOL_READ").is_none());
    }
}
