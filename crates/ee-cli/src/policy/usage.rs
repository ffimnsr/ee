//! Session-local persistent-rule usage ledger (Phase 2 foundation, Phase 6
//! lifecycle).
//!
//! Successful trusted dispatches are counted per
//! `(workspace identity, session_id, rule_id)`.  Counters are session-local
//! and are never written into the trust-store document; rows die with the
//! session.  Failed, canceled, denied, or disconnected requests never
//! consume budget.

use std::collections::BTreeMap;

use super::{UsageSnapshot, WorkspaceIdentity};

/// Successful-use counters keyed by `(workspace, session_id, rule_id)`.
#[derive(Debug, Clone, Default)]
pub(crate) struct UsageLedger {
    used: BTreeMap<(WorkspaceIdentity, String, String), u64>,
}

impl UsageLedger {
    /// Injected evaluator snapshot for one workspace and session.
    pub(crate) fn snapshot(&self, workspace: WorkspaceIdentity, session_id: &str) -> UsageSnapshot {
        let used: BTreeMap<String, u64> = self
            .used
            .iter()
            .filter(|((ws, session, _), _)| *ws == workspace && session == session_id)
            .map(|((_, _, rule_id), count)| (rule_id.clone(), *count))
            .collect();
        UsageSnapshot::new(used)
    }

    /// Records one successful trusted dispatch.
    pub(crate) fn record_use(
        &mut self,
        workspace: WorkspaceIdentity,
        session_id: &str,
        rule_id: &str,
    ) {
        *self.used.entry((workspace, session_id.to_string(), rule_id.to_string())).or_default() +=
            1;
    }

    /// Successful uses recorded for one workspace, session, and rule.
    pub(crate) fn used(
        &self,
        workspace: WorkspaceIdentity,
        session_id: &str,
        rule_id: &str,
    ) -> u64 {
        self.used
            .get(&(workspace, session_id.to_string(), rule_id.to_string()))
            .copied()
            .unwrap_or(0)
    }

    /// Drops every row for `session_id` (session close / connection loss).
    pub(crate) fn invalidate_session(&mut self, session_id: &str) {
        self.used.retain(|(_, session, _), _| session != session_id);
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.used.is_empty()
    }
}
