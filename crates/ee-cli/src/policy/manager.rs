//! Typed trust-rule management, redacted inspection, and side-effect-free testing.

use std::time::SystemTime;

use super::evaluator::{EvaluationResult, PolicyInput, evaluate_with_trace};
use super::rules::TrustRule;
use super::store::{ManagedTrustDocument, TrustStore, TrustStoreError};
use super::{TrustEffect, UsageSnapshot};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RuleMutation {
    Disable,
    Enable,
    Revoke,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RuleSummary {
    pub(crate) id: String,
    pub(crate) effect: TrustEffect,
    pub(crate) matcher: &'static str,
    pub(crate) workspace: String,
    pub(crate) agent: String,
    pub(crate) source: String,
    pub(crate) created_at: String,
    pub(crate) expires_at: String,
    pub(crate) remaining_uses: String,
    pub(crate) enabled: bool,
    pub(crate) last_safe_usage: String,
}

impl RuleSummary {
    pub(crate) fn display(&self) -> String {
        format!(
            "id:{} effect:{} matcher:{} workspace:{} agent:{} source:{} created:{} expires:{} remaining-uses:{} enabled:{} last-safe-usage:{}",
            self.id,
            effect_label(self.effect),
            self.matcher,
            self.workspace,
            self.agent,
            self.source,
            self.created_at,
            self.expires_at,
            self.remaining_uses,
            self.enabled,
            self.last_safe_usage,
        )
    }
}

pub(crate) fn summarize_rules(
    managed: &ManagedTrustDocument,
    usage: &UsageSnapshot,
) -> Vec<RuleSummary> {
    let mut summaries = managed
        .document
        .rules
        .iter()
        .map(|rule| summarize_rule(rule, managed, usage))
        .collect::<Vec<_>>();
    summaries.sort_by(|left, right| {
        effect_rank(left.effect)
            .cmp(&effect_rank(right.effect))
            .then_with(|| left.id.cmp(&right.id))
    });
    summaries
}

fn summarize_rule(
    rule: &TrustRule,
    managed: &ManagedTrustDocument,
    usage: &UsageSnapshot,
) -> RuleSummary {
    let state = managed.state(rule.id());
    let used = usage.used(rule.id());
    let scope = rule.scope();
    RuleSummary {
        id: rule.id().to_string(),
        effect: rule.effect(),
        matcher: matcher_label(rule),
        workspace: scope.workspace.as_string(),
        agent: scope.agent.clone().unwrap_or_else(|| "workspace-any-agent".into()),
        source: rule
            .template_id()
            .map(|template| format!("template:{template}"))
            .unwrap_or_else(|| state.map_or_else(|| "legacy".into(), |state| state.source.clone())),
        created_at: state.map_or_else(|| "unknown".into(), |state| state.created_at.clone()),
        expires_at: scope.expires_at.map_or_else(
            || "never".into(),
            |time| {
                let datetime: chrono::DateTime<chrono::Utc> = time.into();
                datetime.to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
            },
        ),
        remaining_uses: scope.max_uses.map_or_else(
            || "unbounded-removal-only".into(),
            |max| max.saturating_sub(used).to_string(),
        ),
        enabled: state.is_none_or(|state| state.enabled),
        last_safe_usage: if used == 0 {
            "none-this-session".into()
        } else {
            format!("successful-dispatch-count:{used}")
        },
    }
}

pub(crate) fn inspect_rule(
    managed: &ManagedTrustDocument,
    usage: &UsageSnapshot,
    id: &str,
) -> Option<RuleSummary> {
    managed
        .document
        .rules
        .iter()
        .find(|rule| rule.id() == id)
        .map(|rule| summarize_rule(rule, managed, usage))
}

pub(crate) fn mutate_rule(
    store: &TrustStore,
    id: &str,
    mutation: RuleMutation,
    now: SystemTime,
) -> Result<ManagedTrustDocument, TrustStoreError> {
    store.mutate_rule_at(id, mutation, now)
}

/// Pure tester path. Same evaluator, immutable inputs, no store/session/usage mutation.
pub(crate) fn test_policy(input: &PolicyInput<'_>) -> EvaluationResult {
    evaluate_with_trace(input)
}

fn matcher_label(rule: &TrustRule) -> &'static str {
    match rule.untemplated() {
        TrustRule::Command(_) => "command-structured",
        TrustRule::Mcp(_) => "mcp-exact",
        TrustRule::ReadPath(_) => "read-path-prefix",
        TrustRule::McpRead(_) => "mcp-read-path-prefix",
        TrustRule::McpReadProfile(_) => "mcp-read-profile",
        TrustRule::Profile(_) => "curated-profile",
        TrustRule::Write(_) => "write-path-prefix",
        TrustRule::Network(_) => "network-structured",
        TrustRule::McpDeny(_) => "mcp-identity",
        TrustRule::Filesystem(_) => "filesystem-structured",
        TrustRule::Tool(_) => "tool-or-category",
        TrustRule::Template { .. } => unreachable!("untemplated rule"),
    }
}

fn effect_label(effect: TrustEffect) -> &'static str {
    match effect {
        TrustEffect::Allow => "allow",
        TrustEffect::Deny => "deny",
        TrustEffect::Confirm => "confirm",
    }
}

fn effect_rank(effect: TrustEffect) -> u8 {
    match effect {
        TrustEffect::Deny => 0,
        TrustEffect::Confirm => 1,
        TrustEffect::Allow => 2,
    }
}
