//! Agent-permission alignment (Phases 1–3).
//!
//! When an external agent asks the user for permission through ACP
//! (`session/request_permission`) and the user grants it, the bridge must not
//! immediately ask for the same operation again (`fs/write_text_file`,
//! `terminal/create`).  This ledger remembers the user's decision keyed by the
//! ee-validated operation — never by agent-supplied display text — and the
//! approval pump resolves a matching bridge prompt without UI.
//!
//! Modes (`[agents.approval] alignment`):
//! - `exact` (default): only a byte-exact operation match resolves.
//! - `heuristic`: a decision whose payload had no ee-validated identity covers
//!   the next bridge operation of the same coarse class.
//! - `off`: alignment is disabled entirely.
//!
//! Fail closed: in `exact` mode, payloads that cannot be normalized to an
//! ee-validated operation record no grant; `*_once` grants are single-use and
//! expire; `*_always` grants last for the session; grants never cross
//! sessions, and mandatory-confirm rules, safeguards, deny rules, unknown
//! operations, write batches, shell command text, and non-matching requests
//! always keep the explicit prompt.

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use ee_agent_protocol::{PermissionOptionKind, ToolCallUpdate, ToolKind};

use super::super::*;

use crate::config::AgentAlignmentMode;
use crate::policy::{DecisionReason, TrustDecision, TrustOperation, validate_command_tokens};

use super::approval::{ApprovalChoice, ApprovalKind};
use super::prompt::ApprovalPrompt;

/// How long a single-use alignment grant stays valid.  The bridge request the
/// agent proposed permission for normally follows immediately; anything later
/// is treated as a fresh operation and prompts again.
const ONCE_GRANT_TTL: Duration = Duration::from_secs(120);

/// Upper bound on remembered grants; the oldest entry is dropped first so a
/// misbehaving agent cannot grow the ledger without bound.
const MAX_GRANTS: usize = 64;

/// ee-validated identity of one agent-proposed operation.
///
/// Matching is exact on both sides: full content for writes (a different
/// payload at the same path never aligns) and structured argv for terminals.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum AgentOperationKey {
    /// Full-file write: canonical in-workspace path plus exact content.
    Write { path: PathBuf, content: String },
    /// Structured terminal invocation: executable plus argv tokens.
    Terminal { executable: String, argv: Vec<String> },
}

/// One alignment decision recorded from the agent's own permission prompt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AgentGrantDecision {
    Allow,
    Deny,
}

/// Coarse operation class used by heuristic alignment when a payload carries
/// no ee-validated identity.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) enum AgentGrantClass {
    Write,
    Terminal,
}

impl AgentGrantClass {
    /// Short label shown in the permission prompt.
    #[must_use]
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Write => "write",
            Self::Terminal => "terminal",
        }
    }
}

impl AgentOperationKey {
    /// Coarse class of a verified operation.
    #[must_use]
    pub(crate) fn class(&self) -> AgentGrantClass {
        match self {
            Self::Write { .. } => AgentGrantClass::Write,
            Self::Terminal { .. } => AgentGrantClass::Terminal,
        }
    }
}

/// Coarse class of an agent-proposed operation from the ACP tool kind.
/// `None` means the class is unknown; a heuristic grant then covers any class.
#[must_use]
fn class_from_tool_kind(kind: Option<ToolKind>) -> Option<AgentGrantClass> {
    match kind? {
        ToolKind::Execute => Some(AgentGrantClass::Terminal),
        ToolKind::Edit | ToolKind::Delete | ToolKind::Move => Some(AgentGrantClass::Write),
        _ => None,
    }
}

/// Grant identity: an exact ee-validated operation, or a heuristic class slot.
#[derive(Debug, Clone, PartialEq, Eq)]
enum GrantKey {
    Exact(AgentOperationKey),
    /// `None` matches any class (the payload gave no tool kind).
    Heuristic {
        class: Option<AgentGrantClass>,
    },
}

/// Result of one heuristic-class lookup.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum HeuristicMatch {
    None,
    Grant,
    Candidate,
}

#[derive(Debug, Clone)]
struct AgentGrant {
    session_id: String,
    key: GrantKey,
    decision: AgentGrantDecision,
    /// Single-use grant (expires); `false` = rest of the session.
    once: bool,
    /// Counting-only entry: never resolves, only measures how often heuristic
    /// alignment would have applied.
    candidate: bool,
    recorded_at: SystemTime,
}

/// Maps a permission option kind onto grant decisions.  `once` = single-use,
/// otherwise the grant lasts for the session.  Unknown kinds record nothing.
#[must_use]
pub(crate) fn grant_for_option_kind(
    kind: PermissionOptionKind,
) -> Option<(AgentGrantDecision, bool)> {
    match kind {
        PermissionOptionKind::AllowOnce => Some((AgentGrantDecision::Allow, true)),
        PermissionOptionKind::AllowAlways => Some((AgentGrantDecision::Allow, false)),
        PermissionOptionKind::RejectOnce => Some((AgentGrantDecision::Deny, true)),
        PermissionOptionKind::RejectAlways => Some((AgentGrantDecision::Deny, false)),
        _ => None,
    }
}

/// Bounded, session-scoped ledger of agent-proposed decisions.
#[derive(Debug, Default)]
pub(crate) struct AgentPermissionLedger {
    grants: Vec<AgentGrant>,
}

impl AgentPermissionLedger {
    /// Records the decision the user made on the agent's own permission
    /// request.  `*_once` options become single-use grants; `*_always`
    /// options last for the rest of the session.  Unknown option kinds record
    /// nothing.
    pub(crate) fn record_agent_option(
        &mut self,
        session_id: &str,
        key: AgentOperationKey,
        kind: PermissionOptionKind,
        now: SystemTime,
    ) {
        let Some((decision, once)) = grant_for_option_kind(kind) else {
            return;
        };
        self.prune(now);
        // An identical re-request replaces the old grant instead of stacking
        // duplicates.
        let key = GrantKey::Exact(key);
        self.grants.retain(|grant| {
            !(grant.session_id == session_id && grant.key == key && grant.decision == decision)
        });
        if self.grants.len() >= MAX_GRANTS {
            self.grants.remove(0);
        }
        self.grants.push(AgentGrant {
            session_id: session_id.to_string(),
            key,
            decision,
            once,
            candidate: false,
            recorded_at: now,
        });
    }

    /// Records a heuristic grant — or a counting-only candidate — for one
    /// agent-prompt decision whose payload had no ee-validated identity.
    /// Candidates never resolve anything; they only measure how often
    /// heuristic alignment would have applied in `exact` mode.
    pub(crate) fn record_heuristic_decision(
        &mut self,
        session_id: &str,
        class: Option<AgentGrantClass>,
        decision: AgentGrantDecision,
        once: bool,
        candidate: bool,
        now: SystemTime,
    ) {
        self.prune(now);
        let key = GrantKey::Heuristic { class };
        self.grants.retain(|grant| {
            !(grant.session_id == session_id
                && grant.key == key
                && grant.decision == decision
                && grant.candidate == candidate)
        });
        if self.grants.len() >= MAX_GRANTS {
            self.grants.remove(0);
        }
        self.grants.push(AgentGrant {
            session_id: session_id.to_string(),
            key,
            decision,
            once,
            candidate,
            recorded_at: now,
        });
    }

    /// Matches and consumes one exact grant.  Single-use grants are removed on
    /// the first match; session grants stay.  Callers check denies first so a
    /// deny always wins over an allow for the same key.
    pub(crate) fn take_matching(
        &mut self,
        session_id: &str,
        key: &AgentOperationKey,
        decision: AgentGrantDecision,
        now: SystemTime,
    ) -> bool {
        self.prune(now);
        let Some(index) = self.grants.iter().position(|grant| {
            grant.session_id == session_id
                && grant.decision == decision
                && grant.key == GrantKey::Exact(key.clone())
        }) else {
            return false;
        };
        if self.grants[index].once {
            self.grants.remove(index);
        }
        true
    }

    /// Matches one heuristic grant (or candidate) by class: a class-less
    /// grant covers any operation.  Candidates and single-use grants are
    /// consumed on first match; session grants stay.
    pub(crate) fn take_matching_heuristic(
        &mut self,
        session_id: &str,
        class: AgentGrantClass,
        decision: AgentGrantDecision,
        now: SystemTime,
    ) -> HeuristicMatch {
        self.prune(now);
        let Some(index) = self.grants.iter().position(|grant| {
            grant.session_id == session_id
                && grant.decision == decision
                && heuristic_grant_covers(&grant.key, class)
        }) else {
            return HeuristicMatch::None;
        };
        let grant = &self.grants[index];
        let outcome =
            if grant.candidate { HeuristicMatch::Candidate } else { HeuristicMatch::Grant };
        if grant.candidate || grant.once {
            self.grants.remove(index);
        }
        outcome
    }

    /// Counting-only probe: consumes a matching candidate and reports it.
    /// Real heuristic grants are never touched, so switching modes cannot
    /// leak a heuristic grant into `exact` behaviour.
    pub(crate) fn observe_candidate(
        &mut self,
        session_id: &str,
        class: AgentGrantClass,
        decision: AgentGrantDecision,
        now: SystemTime,
    ) -> bool {
        self.prune(now);
        let Some(index) = self.grants.iter().position(|grant| {
            grant.session_id == session_id
                && grant.decision == decision
                && grant.candidate
                && heuristic_grant_covers(&grant.key, class)
        }) else {
            return false;
        };
        self.grants.remove(index);
        true
    }

    /// Drops every grant recorded for `session_id` (session close / deletion).
    pub(crate) fn invalidate_session(&mut self, session_id: &str) {
        self.grants.retain(|grant| grant.session_id != session_id);
    }

    /// Drops every grant (host rebuild).
    pub(crate) fn clear(&mut self) {
        self.grants.clear();
    }

    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.grants.len()
    }

    fn prune(&mut self, now: SystemTime) {
        self.grants.retain(|grant| {
            if !grant.once && !grant.candidate {
                return true;
            }
            match now.duration_since(grant.recorded_at) {
                Ok(elapsed) => elapsed <= ONCE_GRANT_TTL,
                // Clock skew: keep the grant; it still requires an exact
                // operation match and is consumed on first use.
                Err(_) => true,
            }
        });
    }
}

/// Whether one heuristic key covers the requested operation class.
fn heuristic_grant_covers(key: &GrantKey, class: AgentGrantClass) -> bool {
    match key {
        GrantKey::Heuristic { class: granted } => granted.is_none_or(|granted| granted == class),
        GrantKey::Exact(_) => false,
    }
}

/// Outcome of consulting the ledger for one queued bridge approval.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum AgentPermissionAlignment {
    None,
    Allow,
    Deny,
}

/// Strict tokenizer for agent-proposed command text.
///
/// Only plain argv text may align: shell syntax, quoting, or control
/// characters mean the executed command can differ from the payload text, so
/// no grant is recorded and the bridge keeps its own prompt.
fn tokenize_command_text(text: &str) -> Option<Vec<String>> {
    let tokens: Vec<String> = text.split_whitespace().map(str::to_string).collect();
    if tokens.is_empty() {
        return None;
    }
    if tokens.iter().any(|token| {
        token.chars().any(|c| c.is_control() || c == '\u{0}' || ";&|()<>$`'\"\\".contains(c))
    }) {
        return None;
    }
    Some(tokens)
}

/// Terminal operation key from an agent permission payload (`rawInput`).
///
/// Accepts a plain command string or an argv array; both are validated with
/// the same rules used for real terminal requests.
fn terminal_key_from_raw_input(raw_input: &serde_json::Value) -> Option<AgentOperationKey> {
    let command = raw_input.get("command")?;
    let tokens = match command {
        serde_json::Value::String(text) => tokenize_command_text(text)?,
        serde_json::Value::Array(items) => {
            let tokens: Vec<String> = items
                .iter()
                .map(|item| item.as_str().map(str::to_string))
                .collect::<Option<_>>()?;
            if tokens.is_empty() {
                return None;
            }
            tokens
        }
        _ => return None,
    };
    let (executable, argv) = tokens.split_first()?;
    validate_command_tokens(executable, argv).ok()?;
    Some(AgentOperationKey::Terminal { executable: executable.clone(), argv: argv.to_vec() })
}

/// Write payload from an agent permission payload: a path plus the full
/// content the agent claims it will write.  A payload without content can
/// never align (fail closed).
fn write_payload_from_raw_input(raw_input: &serde_json::Value) -> Option<(PathBuf, String)> {
    let path = ["file_path", "filePath", "path"]
        .iter()
        .find_map(|key| raw_input.get(*key).and_then(|value| value.as_str()))
        .map(PathBuf::from)?;
    let content = raw_input.get("content")?.as_str()?.to_string();
    Some((path, content))
}

/// Privacy-safe alignment counters surfaced by `/permissions`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct AgentAlignmentStats {
    /// Exact grants resolved a bridge request without UI.
    pub(crate) exact_resolved: u64,
    /// Heuristic grants resolved a bridge request without UI.
    pub(crate) heuristic_resolved: u64,
    /// Keyless agent decisions recorded as would-resolve candidates.
    pub(crate) heuristic_candidates: u64,
    /// Candidates a bridge request actually matched (dry-run count).
    pub(crate) heuristic_would_resolve: u64,
    /// Bridge approvals that reached the explicit UI prompt.
    pub(crate) prompts_queued: u64,
}

impl AgentAlignmentStats {
    /// One-line summary for `/permissions`; counts only, never paths or text.
    #[must_use]
    pub(crate) fn summary_line(self, mode: AgentAlignmentMode) -> String {
        format!(
            "alignment mode:{} exact-resolved:{} heuristic-resolved:{} heuristic-candidates:{} heuristic-would-resolve:{} bridge-prompts:{}",
            mode.label(),
            self.exact_resolved,
            self.heuristic_resolved,
            self.heuristic_candidates,
            self.heuristic_would_resolve,
            self.prompts_queued,
        )
    }
}

/// Terminal operation key from an agent permission payload (`rawInput`).
impl App {
    /// Canonical in-workspace path for one agent-proposed path, or `None`
    /// when it is relative, inaccessible, or outside every allowed root.
    fn agent_operation_write_path(&self, path: &Path) -> Option<PathBuf> {
        let candidate = self.canonical_workspace_path(path).ok()?;
        self.allowed_fs_roots().iter().any(|root| candidate.starts_with(root)).then_some(candidate)
    }

    /// Best-effort ee-validated identity of the operation the agent just asked
    /// permission for.  `None` means "unknown": no grant is recorded and the
    /// bridge always prompts.
    pub(crate) fn agent_operation_key_for_tool_call(
        &self,
        tool_call: &ToolCallUpdate,
    ) -> Option<AgentOperationKey> {
        let fields = &tool_call.fields;
        if let Some(raw_input) = fields.raw_input.as_ref() {
            if let Some(key) = terminal_key_from_raw_input(raw_input) {
                return Some(key);
            }
            if let Some((path, content)) = write_payload_from_raw_input(raw_input)
                && let Some(path) = self.agent_operation_write_path(&path)
            {
                return Some(AgentOperationKey::Write { path, content });
            }
        }
        let content = fields
            .raw_input
            .as_ref()
            .and_then(|raw| raw.get("content"))
            .and_then(|content| content.as_str())?;
        let path = self.agent_operation_write_path(&fields.locations.as_ref()?.first()?.path)?;
        Some(AgentOperationKey::Write { path, content: content.to_string() })
    }

    /// Coarse class of the operation the agent asked permission for (from the
    /// structured tool kind); used only by heuristic alignment.
    pub(crate) fn agent_operation_class_for_tool_call(
        &self,
        tool_call: &ToolCallUpdate,
    ) -> Option<AgentGrantClass> {
        class_from_tool_kind(tool_call.fields.kind)
    }

    /// ee-validated identity of one queued bridge approval, when the prompt
    /// covers exactly one write or one structured terminal invocation.
    fn agent_operation_key_for_prompt(&self, prompt: &ApprovalPrompt) -> Option<AgentOperationKey> {
        match &prompt.kind {
            ApprovalKind::Write { path, content, .. } => {
                let path = self.agent_operation_write_path(path)?;
                Some(AgentOperationKey::Write { path, content: content.clone() })
            }
            ApprovalKind::TerminalCreate { request } => {
                let invocation = self.command_invocation_for_request(request).ok()?;
                Some(AgentOperationKey::Terminal {
                    executable: invocation.executable,
                    argv: invocation.argv,
                })
            }
            _ => None,
        }
    }

    /// Consumes one alignment grant for the queued prompt, if any, according
    /// to the configured `[agents.approval] alignment` mode.  Denies win over
    /// allows for the same operation, matching session-policy precedence.
    /// `exact` mode never resolves on a heuristic grant; it only records
    /// would-resolve candidate counts.
    pub(super) fn agent_permission_alignment(
        &mut self,
        prompt: &ApprovalPrompt,
    ) -> AgentPermissionAlignment {
        let mode = self.config.agents.approval.alignment;
        if mode == AgentAlignmentMode::Off {
            return AgentPermissionAlignment::None;
        }
        let Some(key) = self.agent_operation_key_for_prompt(prompt) else {
            return AgentPermissionAlignment::None;
        };
        let now = self.trust_clock.now();
        let session_id = prompt.session_id.as_str();
        if self.agents.agent_permissions.take_matching(
            session_id,
            &key,
            AgentGrantDecision::Deny,
            now,
        ) {
            self.agents.alignment_stats.exact_resolved += 1;
            return AgentPermissionAlignment::Deny;
        }
        if self.agents.agent_permissions.take_matching(
            session_id,
            &key,
            AgentGrantDecision::Allow,
            now,
        ) {
            self.agents.alignment_stats.exact_resolved += 1;
            return AgentPermissionAlignment::Allow;
        }
        let class = key.class();
        if mode == AgentAlignmentMode::Heuristic {
            match self.agents.agent_permissions.take_matching_heuristic(
                session_id,
                class,
                AgentGrantDecision::Deny,
                now,
            ) {
                HeuristicMatch::Grant => {
                    self.agents.alignment_stats.heuristic_resolved += 1;
                    return AgentPermissionAlignment::Deny;
                }
                HeuristicMatch::Candidate => {
                    self.agents.alignment_stats.heuristic_would_resolve += 1;
                }
                HeuristicMatch::None => {}
            }
            match self.agents.agent_permissions.take_matching_heuristic(
                session_id,
                class,
                AgentGrantDecision::Allow,
                now,
            ) {
                HeuristicMatch::Grant => {
                    self.agents.alignment_stats.heuristic_resolved += 1;
                    return AgentPermissionAlignment::Allow;
                }
                HeuristicMatch::Candidate => {
                    self.agents.alignment_stats.heuristic_would_resolve += 1;
                }
                HeuristicMatch::None => {}
            }
            return AgentPermissionAlignment::None;
        }
        // Exact mode: candidates only measure what heuristic mode would do.
        let deny_hit = self.agents.agent_permissions.observe_candidate(
            session_id,
            class,
            AgentGrantDecision::Deny,
            now,
        );
        let allow_hit = !deny_hit
            && self.agents.agent_permissions.observe_candidate(
                session_id,
                class,
                AgentGrantDecision::Allow,
                now,
            );
        if deny_hit || allow_hit {
            self.agents.alignment_stats.heuristic_would_resolve += 1;
        }
        AgentPermissionAlignment::None
    }

    /// Dispatches an aligned allow with its own audit reason and transcript
    /// notice so the automatic decision stays visible.
    pub(super) fn resolve_agent_permission_allow(
        &mut self,
        prompt: ApprovalPrompt,
        operation: &TrustOperation,
        session_id: &str,
    ) {
        let decision = TrustDecision::allow(DecisionReason::AgentPermissionAllow, None);
        self.push_trust_audit(operation, &decision, session_id);
        let summary = String::from("auto-approved (agent permission already granted)");
        if let Some(thread_index) = prompt.thread_index
            && let Some(thread) = self.agents.threads.get_mut(thread_index)
        {
            thread.push_system(summary.clone());
        }
        self.backend.status_message = Some(summary);
        self.resolve_approval(prompt, ApprovalChoice::AllowOnce);
    }

    /// Denies a bridge request the user already rejected on the agent's own
    /// permission prompt.
    pub(super) fn resolve_agent_permission_deny(
        &mut self,
        prompt: ApprovalPrompt,
        operation: &TrustOperation,
        session_id: &str,
    ) {
        let decision = TrustDecision::deny(DecisionReason::AgentPermissionDeny, None);
        self.push_trust_audit(operation, &decision, session_id);
        self.resolve_policy_deny(prompt, &decision);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base_now() -> SystemTime {
        SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000)
    }

    fn write_key(path: &str, content: &str) -> AgentOperationKey {
        AgentOperationKey::Write { path: PathBuf::from(path), content: content.to_string() }
    }

    fn terminal_key(executable: &str, args: &[&str]) -> AgentOperationKey {
        AgentOperationKey::Terminal {
            executable: executable.to_string(),
            argv: args.iter().map(|arg| (*arg).to_string()).collect(),
        }
    }

    #[test]
    fn allow_once_is_single_use_and_session_scoped() {
        let mut ledger = AgentPermissionLedger::default();
        let now = base_now();
        ledger.record_agent_option(
            "s1",
            write_key("/w/a.txt", "a"),
            PermissionOptionKind::AllowOnce,
            now,
        );

        assert!(ledger.take_matching(
            "s1",
            &write_key("/w/a.txt", "a"),
            AgentGrantDecision::Allow,
            now
        ));
        assert!(!ledger.take_matching(
            "s1",
            &write_key("/w/a.txt", "a"),
            AgentGrantDecision::Allow,
            now
        ));
        // A grant recorded for one session never leaks into another.
        ledger.record_agent_option(
            "s1",
            terminal_key("echo", &["hi"]),
            PermissionOptionKind::AllowOnce,
            now,
        );
        assert!(!ledger.take_matching(
            "s2",
            &terminal_key("echo", &["hi"]),
            AgentGrantDecision::Allow,
            now
        ));
    }

    #[test]
    fn allow_always_lasts_for_the_session_and_clears_on_invalidate() {
        let mut ledger = AgentPermissionLedger::default();
        let now = base_now();
        ledger.record_agent_option(
            "s1",
            terminal_key("cargo", &["test"]),
            PermissionOptionKind::AllowAlways,
            now,
        );

        for _ in 0..3 {
            assert!(ledger.take_matching(
                "s1",
                &terminal_key("cargo", &["test"]),
                AgentGrantDecision::Allow,
                now
            ));
        }
        ledger.invalidate_session("s1");
        assert_eq!(ledger.len(), 0);
        assert!(!ledger.take_matching(
            "s1",
            &terminal_key("cargo", &["test"]),
            AgentGrantDecision::Allow,
            now
        ));
    }

    #[test]
    fn once_grants_expire_after_the_ttl() {
        let mut ledger = AgentPermissionLedger::default();
        let now = base_now();
        ledger.record_agent_option(
            "s1",
            write_key("/w/a.txt", "a"),
            PermissionOptionKind::AllowOnce,
            now,
        );

        let later = now + ONCE_GRANT_TTL + Duration::from_secs(1);
        assert!(!ledger.take_matching(
            "s1",
            &write_key("/w/a.txt", "a"),
            AgentGrantDecision::Allow,
            later
        ));
        assert_eq!(ledger.len(), 0);
    }

    #[test]
    fn writes_require_exact_content_and_terminals_exact_argv() {
        let mut ledger = AgentPermissionLedger::default();
        let now = base_now();
        ledger.record_agent_option(
            "s1",
            write_key("/w/a.txt", "approved"),
            PermissionOptionKind::AllowOnce,
            now,
        );
        assert!(!ledger.take_matching(
            "s1",
            &write_key("/w/a.txt", "different"),
            AgentGrantDecision::Allow,
            now
        ));
        assert!(ledger.take_matching(
            "s1",
            &write_key("/w/a.txt", "approved"),
            AgentGrantDecision::Allow,
            now
        ));

        ledger.record_agent_option(
            "s1",
            terminal_key("echo", &["hi"]),
            PermissionOptionKind::AllowOnce,
            now,
        );
        assert!(!ledger.take_matching(
            "s1",
            &terminal_key("echo", &["hi", "there"]),
            AgentGrantDecision::Allow,
            now
        ));
    }

    #[test]
    fn deny_wins_over_allow_for_the_same_key() {
        let mut ledger = AgentPermissionLedger::default();
        let now = base_now();
        ledger.record_agent_option(
            "s1",
            write_key("/w/a.txt", "a"),
            PermissionOptionKind::AllowOnce,
            now,
        );
        ledger.record_agent_option(
            "s1",
            write_key("/w/a.txt", "a"),
            PermissionOptionKind::RejectOnce,
            now,
        );

        // Callers evaluate deny first; the allow grant survives untouched.
        assert!(ledger.take_matching(
            "s1",
            &write_key("/w/a.txt", "a"),
            AgentGrantDecision::Deny,
            now
        ));
        assert!(ledger.take_matching(
            "s1",
            &write_key("/w/a.txt", "a"),
            AgentGrantDecision::Allow,
            now
        ));
    }

    #[test]
    fn identical_grants_replace_instead_of_stacking() {
        let mut ledger = AgentPermissionLedger::default();
        let now = base_now();
        ledger.record_agent_option(
            "s1",
            write_key("/w/a.txt", "a"),
            PermissionOptionKind::AllowOnce,
            now,
        );
        ledger.record_agent_option(
            "s1",
            write_key("/w/a.txt", "a"),
            PermissionOptionKind::AllowOnce,
            now,
        );
        assert_eq!(ledger.len(), 1);
    }

    #[test]
    fn command_tokenizer_rejects_shell_text_and_wrappers() {
        assert_eq!(
            tokenize_command_text("cargo test"),
            Some(vec![String::from("cargo"), String::from("test")])
        );
        // Quoting, metacharacters, and control characters cannot align.
        assert!(tokenize_command_text("sh -c 'rm -rf /'").is_none());
        assert!(tokenize_command_text("echo hi | tee out").is_none());
        assert!(tokenize_command_text("echo $(whoami)").is_none());
        assert!(tokenize_command_text("echo \"quoted\"").is_none());
        assert!(tokenize_command_text("   ").is_none());

        // Shell wrappers and empty argv arrays never produce a key.
        assert!(
            terminal_key_from_raw_input(&serde_json::json!({ "command": "bash -c x" })).is_none()
        );
        assert!(terminal_key_from_raw_input(&serde_json::json!({ "command": [] })).is_none());
        assert!(
            terminal_key_from_raw_input(&serde_json::json!({ "command": "echo hi" })).is_some()
        );
        assert!(
            terminal_key_from_raw_input(&serde_json::json!({ "command": ["echo", "hi"] }))
                .is_some()
        );
    }

    #[test]
    fn write_payload_requires_path_and_content() {
        assert!(
            write_payload_from_raw_input(
                &serde_json::json!({ "file_path": "/w/a", "content": "x" })
            )
            .is_some()
        );
        assert!(
            write_payload_from_raw_input(&serde_json::json!({ "file_path": "/w/a" })).is_none()
        );
        assert!(write_payload_from_raw_input(&serde_json::json!({ "content": "x" })).is_none());
        assert!(
            write_payload_from_raw_input(&serde_json::json!({ "path": "/w/a", "content": "x" }))
                .is_some()
        );
    }

    #[test]
    fn heuristic_grants_match_by_class_or_any_class() {
        let mut ledger = AgentPermissionLedger::default();
        let now = base_now();
        ledger.record_heuristic_decision(
            "s1",
            Some(AgentGrantClass::Terminal),
            AgentGrantDecision::Allow,
            true,
            false,
            now,
        );
        // Wrong class never matches; the right class consumes the grant.
        assert_eq!(
            ledger.take_matching_heuristic(
                "s1",
                AgentGrantClass::Write,
                AgentGrantDecision::Allow,
                now
            ),
            HeuristicMatch::None
        );
        assert_eq!(
            ledger.take_matching_heuristic(
                "s1",
                AgentGrantClass::Terminal,
                AgentGrantDecision::Allow,
                now
            ),
            HeuristicMatch::Grant
        );
        // A class-less grant covers any class.
        ledger.record_heuristic_decision("s1", None, AgentGrantDecision::Allow, true, false, now);
        assert_eq!(
            ledger.take_matching_heuristic(
                "s1",
                AgentGrantClass::Write,
                AgentGrantDecision::Allow,
                now
            ),
            HeuristicMatch::Grant
        );
    }

    #[test]
    fn heuristic_session_grant_survives_reuse_and_dies_with_the_session() {
        let mut ledger = AgentPermissionLedger::default();
        let now = base_now();
        ledger.record_heuristic_decision(
            "s1",
            Some(AgentGrantClass::Write),
            AgentGrantDecision::Allow,
            false,
            false,
            now,
        );
        for _ in 0..3 {
            assert_eq!(
                ledger.take_matching_heuristic(
                    "s1",
                    AgentGrantClass::Write,
                    AgentGrantDecision::Allow,
                    now
                ),
                HeuristicMatch::Grant
            );
        }
        ledger.invalidate_session("s1");
        assert_eq!(ledger.len(), 0);
    }

    #[test]
    fn candidates_are_counted_but_never_resolve_or_consume_grants() {
        let mut ledger = AgentPermissionLedger::default();
        let now = base_now();
        ledger.record_heuristic_decision("s1", None, AgentGrantDecision::Allow, true, true, now);
        assert!(ledger.observe_candidate(
            "s1",
            AgentGrantClass::Write,
            AgentGrantDecision::Allow,
            now
        ));
        // Consumed on first observation; a second probe finds nothing.
        assert!(!ledger.observe_candidate(
            "s1",
            AgentGrantClass::Write,
            AgentGrantDecision::Allow,
            now
        ));
        // Real grants are invisible to the counting probe.
        ledger.record_heuristic_decision("s1", None, AgentGrantDecision::Allow, true, false, now);
        assert!(!ledger.observe_candidate(
            "s1",
            AgentGrantClass::Write,
            AgentGrantDecision::Allow,
            now
        ));
        assert_eq!(
            ledger.take_matching_heuristic(
                "s1",
                AgentGrantClass::Write,
                AgentGrantDecision::Allow,
                now
            ),
            HeuristicMatch::Grant
        );
    }

    #[test]
    fn class_from_tool_kind_maps_only_structured_kinds() {
        assert_eq!(class_from_tool_kind(Some(ToolKind::Execute)), Some(AgentGrantClass::Terminal));
        assert_eq!(class_from_tool_kind(Some(ToolKind::Edit)), Some(AgentGrantClass::Write));
        assert_eq!(class_from_tool_kind(Some(ToolKind::Delete)), Some(AgentGrantClass::Write));
        assert_eq!(class_from_tool_kind(Some(ToolKind::Read)), None);
        assert_eq!(class_from_tool_kind(None), None);
    }
}
