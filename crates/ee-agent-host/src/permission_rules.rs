//! Declarative permission rules and saved approvals.
//!
//! Mirrors the reference `permission.ts` / `permission/saved.ts` shape: a
//! ruleset is an ordered list of `action`/`resource` wildcard rules whose
//! effect is `allow`, `ask`, or `deny`. Rules are matched last-wins, and a
//! configured deny always wins over any allow. Saved approvals are allow-only
//! rules remembered after the user chooses an `allow_always` option.
//!
//! The ruleset is consulted by [`crate::PermissionBroker`] before it asks the
//! client, so an `allow`/`deny` decision never performs an ACP
//! `session/request_permission` round-trip. Only an `ask` decision reaches the
//! existing approval pipeline, which is where the ee approval classifier runs.

use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};

use ee_agent_protocol::{ToolCallUpdate, ToolKind};
use serde::{Deserialize, Serialize};

/// Effect of one permission rule.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PermissionEffect {
    /// Permit without asking the user.
    Allow,
    /// Ask the user (the default when no rule matches).
    Ask,
    /// Reject without asking the user.
    Deny,
}

impl PermissionEffect {
    /// Parses the config spelling.
    pub fn parse(value: &str) -> Result<Self, PermissionRuleError> {
        match value.trim() {
            "allow" => Ok(Self::Allow),
            "ask" => Ok(Self::Ask),
            "deny" => Ok(Self::Deny),
            other => Err(PermissionRuleError::new(format!(
                "unsupported permission effect {other:?}; expected \"allow\", \"ask\", or \"deny\""
            ))),
        }
    }

    /// Stable config spelling.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Allow => "allow",
            Self::Ask => "ask",
            Self::Deny => "deny",
        }
    }
}

/// Coarse side-effect class derived from the ACP tool kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PermissionSideEffect {
    /// Reading data.
    Read,
    /// Mutating data.
    Write,
    /// Running commands.
    Execute,
    /// Unknown or non-side-effecting.
    Other,
}

impl PermissionSideEffect {
    /// Stable label used as a rule action or `class:<label>` pattern.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Read => "read",
            Self::Write => "write",
            Self::Execute => "execute",
            Self::Other => "other",
        }
    }

    /// Derives the class from the ACP tool kind.
    #[must_use]
    pub fn from_tool_kind(kind: Option<ToolKind>) -> Self {
        match kind {
            Some(ToolKind::Read | ToolKind::Search | ToolKind::Think | ToolKind::Fetch) => {
                Self::Read
            }
            Some(ToolKind::Edit | ToolKind::Delete | ToolKind::Move) => Self::Write,
            Some(ToolKind::Execute) => Self::Execute,
            _ => Self::Other,
        }
    }
}

/// One permission rule with wildcard `action` and `resource` patterns.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PermissionRule {
    /// Tool-name or side-effect-class pattern.
    pub action: String,
    /// Resource pattern (path, command, URL, or `*`).
    pub resource: String,
    /// What the rule decides.
    pub effect: PermissionEffect,
}

impl PermissionRule {
    /// Creates a rule.
    #[must_use]
    pub fn new(
        action: impl Into<String>,
        resource: impl Into<String>,
        effect: PermissionEffect,
    ) -> Self {
        Self { action: action.into(), resource: resource.into(), effect }
    }

    /// Whether this rule matches `target`.
    #[must_use]
    pub fn matches(&self, target: &PermissionTarget) -> bool {
        let class = target.side_effect.label();
        let action_match = wildcard_match(&self.action, &target.action)
            || self.action == class
            || self.action == format!("class:{class}");
        let resource_match = wildcard_match(&self.resource, &target.resource)
            || self.resource == format!("class:{class}");
        action_match && resource_match
    }
}

/// Ordered ruleset. Last matching rule wins; no match means [`PermissionEffect::Ask`].
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PermissionRuleset {
    rules: Vec<PermissionRule>,
}

impl PermissionRuleset {
    /// Creates an empty ruleset (every request asks).
    #[must_use]
    pub fn empty() -> Self {
        Self::default()
    }

    /// Creates a deny-all ruleset. Used when configuration cannot be parsed so
    /// a malformed ruleset can never silently allow an operation.
    #[must_use]
    pub fn deny_all() -> Self {
        Self { rules: vec![PermissionRule::new("*", "*", PermissionEffect::Deny)] }
    }

    /// Creates a ruleset from explicit rules, preserving order.
    #[must_use]
    pub fn new(rules: Vec<PermissionRule>) -> Self {
        Self { rules }
    }

    /// The stored rules in evaluation order.
    #[must_use]
    pub fn rules(&self) -> &[PermissionRule] {
        &self.rules
    }

    /// Whether the ruleset has no rules.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.rules.is_empty()
    }

    /// Evaluates the ruleset for one target (last match wins).
    #[must_use]
    pub fn evaluate(&self, target: &PermissionTarget) -> PermissionEffect {
        self.rules
            .iter()
            .rev()
            .find(|rule| rule.matches(target))
            .map_or(PermissionEffect::Ask, |rule| rule.effect)
    }

    /// Parses a config value.
    ///
    /// Accepted shapes:
    /// - `"allow" | "ask" | "deny"` (one rule for every action/resource)
    /// - `{ "<action>" = "<effect>" , ... }`
    /// - `{ "<action>" = { "<resource>" = "<effect>", ... }, ... }`
    /// - `[{ action = "...", resource = "...", effect = "..." }, ...]`
    ///
    /// Unknown shapes, keys, or effects fail closed with an error.
    pub fn from_value(value: &serde_json::Value) -> Result<Self, PermissionRuleError> {
        match value {
            serde_json::Value::String(effect) => {
                Ok(Self::new(vec![PermissionRule::new("*", "*", PermissionEffect::parse(effect)?)]))
            }
            serde_json::Value::Array(entries) => {
                let mut rules = Vec::with_capacity(entries.len());
                for (index, entry) in entries.iter().enumerate() {
                    rules.push(rule_from_object(entry, index)?);
                }
                Ok(Self::new(rules))
            }
            serde_json::Value::Object(actions) => {
                let mut rules = Vec::new();
                for (action, value) in actions {
                    match value {
                        serde_json::Value::String(effect) => {
                            rules.push(PermissionRule::new(
                                action.clone(),
                                "*",
                                PermissionEffect::parse(effect)?,
                            ));
                        }
                        serde_json::Value::Object(resources) => {
                            for (resource, effect) in resources {
                                let effect = effect.as_str().ok_or_else(|| {
                                    PermissionRuleError::new(format!(
                                        "permission rule {action:?}.{resource:?} must be a string effect"
                                    ))
                                })?;
                                rules.push(PermissionRule::new(
                                    action.clone(),
                                    resource.clone(),
                                    PermissionEffect::parse(effect)?,
                                ));
                            }
                        }
                        other => {
                            return Err(PermissionRuleError::new(format!(
                                "permission rule {action:?} must be an effect string or a resource table, got {other}"
                            )));
                        }
                    }
                }
                Ok(Self::new(rules))
            }
            other => Err(PermissionRuleError::new(format!(
                "permission rules must be an effect string, table, or array, got {other}"
            ))),
        }
    }
}

fn rule_from_object(
    value: &serde_json::Value,
    index: usize,
) -> Result<PermissionRule, PermissionRuleError> {
    let object = value.as_object().ok_or_else(|| {
        PermissionRuleError::new(format!("permission rule {index} must be a table"))
    })?;
    for key in object.keys() {
        if !matches!(key.as_str(), "action" | "resource" | "effect") {
            return Err(PermissionRuleError::new(format!(
                "permission rule {index} has unknown key {key:?}"
            )));
        }
    }
    let action = object.get("action").and_then(serde_json::Value::as_str).ok_or_else(|| {
        PermissionRuleError::new(format!("permission rule {index} needs an action"))
    })?;
    let resource = object.get("resource").and_then(serde_json::Value::as_str).unwrap_or("*");
    let effect = object.get("effect").and_then(serde_json::Value::as_str).ok_or_else(|| {
        PermissionRuleError::new(format!("permission rule {index} needs an effect"))
    })?;
    Ok(PermissionRule::new(action, resource, PermissionEffect::parse(effect)?))
}

/// Wildcard matcher supporting `*` (any sequence) and exact text.
#[must_use]
pub fn wildcard_match(pattern: &str, text: &str) -> bool {
    if pattern == "*" {
        return true;
    }
    let pattern: Vec<char> = pattern.chars().collect();
    let text: Vec<char> = text.chars().collect();
    let (mut pi, mut ti) = (0usize, 0usize);
    let (mut star, mut resume) = (None, 0usize);
    while ti < text.len() {
        if pi < pattern.len() && pattern[pi] == text[ti] {
            pi += 1;
            ti += 1;
        } else if pi < pattern.len() && pattern[pi] == '*' {
            star = Some(pi);
            pi += 1;
            resume = ti;
        } else if let Some(index) = star {
            pi = index + 1;
            resume += 1;
            ti = resume;
        } else {
            return false;
        }
    }
    while pi < pattern.len() && pattern[pi] == '*' {
        pi += 1;
    }
    pi == pattern.len()
}

/// One permission request normalized for rule matching.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PermissionTarget {
    /// Programmatic tool name, falling back to the side-effect class label.
    pub action: String,
    /// Most specific resource the request names, or `*`.
    pub resource: String,
    /// Coarse side-effect class.
    pub side_effect: PermissionSideEffect,
}

impl PermissionTarget {
    /// Normalizes one ACP tool-call update.
    #[must_use]
    pub fn from_tool_call(tool_call: &ToolCallUpdate) -> Self {
        let fields = &tool_call.fields;
        let side_effect = PermissionSideEffect::from_tool_kind(fields.kind);
        let action = fields
            .name
            .as_deref()
            .filter(|name| !name.trim().is_empty())
            .map(str::to_string)
            .unwrap_or_else(|| side_effect.label().to_string());
        let resource = resource_of(fields);
        Self { action, resource, side_effect }
    }
}

fn resource_of(fields: &ee_agent_protocol::ToolCallUpdateFields) -> String {
    if let Some(path) = fields
        .locations
        .as_ref()
        .and_then(|locations| locations.first())
        .map(|location| location.path.display().to_string())
    {
        return path;
    }
    if let Some(input) = fields.raw_input.as_ref().and_then(serde_json::Value::as_object) {
        for key in ["command", "path", "url"] {
            if let Some(value) = input.get(key).and_then(serde_json::Value::as_str) {
                return value.to_string();
            }
        }
    }
    fields.title.as_deref().filter(|title| !title.trim().is_empty()).unwrap_or("*").to_string()
}

/// Configured rules plus allow-only saved approvals.
#[derive(Debug, Clone, Default)]
pub struct PermissionRules {
    configured: PermissionRuleset,
    saved: PermissionRuleset,
    persistence: Option<PathBuf>,
}

impl PermissionRules {
    /// Creates rules with configured rules and no saved approvals.
    #[must_use]
    pub fn new(configured: PermissionRuleset) -> Self {
        Self { configured, saved: PermissionRuleset::empty(), persistence: None }
    }

    /// Loads saved approvals from `path`, then attaches persistence so future
    /// `allow_always` decisions are written back. A malformed file is treated
    /// as empty (fail closed: nothing is auto-allowed) and logged.
    #[must_use]
    pub fn with_persistence(mut self, path: impl Into<PathBuf>) -> Self {
        let path = path.into();
        match fs::read_to_string(&path) {
            Ok(text) => match serde_json::from_str::<SavedPermissions>(&text) {
                Ok(saved) => self.saved = PermissionRuleset::new(saved.rules),
                Err(error) => {
                    tracing::warn!(%error, path = %path.display(), "ignoring malformed saved permissions");
                }
            },
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                tracing::warn!(%error, path = %path.display(), "could not read saved permissions");
            }
        }
        self.persistence = Some(path);
        self
    }

    /// The configured ruleset.
    #[must_use]
    pub fn configured(&self) -> &PermissionRuleset {
        &self.configured
    }

    /// The saved allow rules.
    #[must_use]
    pub fn saved(&self) -> &PermissionRuleset {
        &self.saved
    }

    /// Decides one target.
    ///
    /// A configured deny always wins; a configured allow permits immediately;
    /// otherwise a saved allow permits; otherwise the request asks.
    #[must_use]
    pub fn decide(&self, target: &PermissionTarget) -> PermissionEffect {
        match self.configured.evaluate(target) {
            PermissionEffect::Deny => PermissionEffect::Deny,
            PermissionEffect::Allow => PermissionEffect::Allow,
            PermissionEffect::Ask => {
                if self.saved.evaluate(target) == PermissionEffect::Allow {
                    PermissionEffect::Allow
                } else {
                    PermissionEffect::Ask
                }
            }
        }
    }

    /// Remembers an `allow_always` decision as a saved allow rule.
    pub fn remember_allow(&mut self, target: &PermissionTarget) {
        let rule = PermissionRule::new(
            target.action.clone(),
            target.resource.clone(),
            PermissionEffect::Allow,
        );
        self.saved.rules.retain(|existing| {
            !(existing.action == rule.action
                && existing.resource == rule.resource
                && existing.effect == PermissionEffect::Allow)
        });
        self.saved.rules.push(rule);
        self.persist();
    }

    fn persist(&self) {
        let Some(path) = self.persistence.as_ref() else { return };
        let saved = SavedPermissions { rules: self.saved.rules.clone() };
        let result =
            serde_json::to_string_pretty(&saved).map_err(std::io::Error::other).and_then(|text| {
                if let Some(parent) = path.parent().filter(|parent| !parent.as_os_str().is_empty())
                {
                    fs::create_dir_all(parent)?;
                }
                fs::write(path, text)
            });
        if let Err(error) = result {
            tracing::warn!(%error, path = %path.display(), "could not persist saved permissions");
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct SavedPermissions {
    #[serde(default)]
    rules: Vec<PermissionRule>,
}

/// Permission-rule configuration error.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PermissionRuleError {
    message: String,
}

impl PermissionRuleError {
    fn new(message: impl Into<String>) -> Self {
        Self { message: message.into() }
    }
}

impl fmt::Display for PermissionRuleError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for PermissionRuleError {}

/// Default saved-approval file name used by the editor config layer.
pub const SAVED_PERMISSIONS_FILE: &str = "agent-permissions.json";

/// Resolves a saved-permissions file under `config_dir`.
#[must_use]
pub fn saved_permissions_path(config_dir: &Path) -> PathBuf {
    config_dir.join(SAVED_PERMISSIONS_FILE)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ee_agent_protocol::{ToolCallLocation, ToolCallUpdateFields};

    fn target(action: &str, resource: &str, class: PermissionSideEffect) -> PermissionTarget {
        PermissionTarget {
            action: action.to_string(),
            resource: resource.to_string(),
            side_effect: class,
        }
    }

    #[test]
    fn wildcard_matching_is_anchored() {
        assert!(wildcard_match("*", "anything"));
        assert!(wildcard_match("cargo *", "cargo test"));
        assert!(wildcard_match("*.rs", "src/lib.rs"));
        assert!(wildcard_match("a*b*c", "axxbyyc"));
        assert!(!wildcard_match("cargo *", "npm test"));
        assert!(!wildcard_match("*.rs", "src/lib.py"));
        assert!(wildcard_match("exact", "exact"));
        assert!(!wildcard_match("exact", "exactly"));
    }

    #[test]
    fn last_matching_rule_wins() {
        let rules = PermissionRuleset::new(vec![
            PermissionRule::new("bash", "*", PermissionEffect::Allow),
            PermissionRule::new("bash", "rm *", PermissionEffect::Deny),
        ]);
        assert_eq!(
            rules.evaluate(&target("bash", "cargo test", PermissionSideEffect::Execute)),
            PermissionEffect::Allow
        );
        assert_eq!(
            rules.evaluate(&target("bash", "rm -rf /", PermissionSideEffect::Execute)),
            PermissionEffect::Deny
        );
    }

    #[test]
    fn side_effect_class_patterns_match() {
        let rules = PermissionRuleset::new(vec![PermissionRule::new(
            "class:write",
            "*",
            PermissionEffect::Deny,
        )]);
        assert_eq!(
            rules.evaluate(&target("apply_patch", "/tmp/x", PermissionSideEffect::Write)),
            PermissionEffect::Deny
        );
        assert_eq!(
            rules.evaluate(&target("read_file", "/tmp/x", PermissionSideEffect::Read)),
            PermissionEffect::Ask
        );
    }

    #[test]
    fn configured_deny_beats_saved_allow() {
        let mut rules = PermissionRules::new(PermissionRuleset::new(vec![PermissionRule::new(
            "bash",
            "rm *",
            PermissionEffect::Deny,
        )]));
        let denied = target("bash", "rm -rf /", PermissionSideEffect::Execute);
        rules.remember_allow(&denied);
        assert_eq!(rules.decide(&denied), PermissionEffect::Deny);
    }

    #[test]
    fn saved_allow_permits_after_ask() {
        let mut rules = PermissionRules::new(PermissionRuleset::empty());
        let ask = target("bash", "cargo test", PermissionSideEffect::Execute);
        assert_eq!(rules.decide(&ask), PermissionEffect::Ask);
        rules.remember_allow(&ask);
        assert_eq!(rules.decide(&ask), PermissionEffect::Allow);
    }

    #[test]
    fn from_value_parses_all_supported_shapes() {
        let scalar = PermissionRuleset::from_value(&serde_json::json!("deny")).expect("scalar");
        assert_eq!(
            scalar.evaluate(&target("*", "*", PermissionSideEffect::Other)),
            PermissionEffect::Deny
        );

        let map = PermissionRuleset::from_value(&serde_json::json!({
            "bash": { "cargo *": "allow", "rm *": "deny" },
            "edit": "ask"
        }))
        .expect("map");
        assert_eq!(
            map.evaluate(&target("bash", "cargo test", PermissionSideEffect::Execute)),
            PermissionEffect::Allow
        );
        assert_eq!(
            map.evaluate(&target("bash", "rm -rf /", PermissionSideEffect::Execute)),
            PermissionEffect::Deny
        );
        assert_eq!(
            map.evaluate(&target("edit", "/tmp/x", PermissionSideEffect::Write)),
            PermissionEffect::Ask
        );

        let array = PermissionRuleset::from_value(&serde_json::json!([
            { "action": "bash", "effect": "allow" },
            { "action": "bash", "resource": "rm *", "effect": "deny" }
        ]))
        .expect("array");
        assert_eq!(
            array.evaluate(&target("bash", "rm -rf /", PermissionSideEffect::Execute)),
            PermissionEffect::Deny
        );
    }

    #[test]
    fn from_value_fails_closed_on_bad_input() {
        assert!(PermissionRuleset::from_value(&serde_json::json!("sometimes")).is_err());
        assert!(PermissionRuleset::from_value(&serde_json::json!({ "bash": 1 })).is_err());
        assert!(PermissionRuleset::from_value(&serde_json::json!([{ "action": "bash" }])).is_err());
        assert!(PermissionRuleset::from_value(&serde_json::json!([{ "nope": "x" }])).is_err());
        assert!(PermissionRuleset::from_value(&serde_json::json!(1)).is_err());
    }

    #[test]
    fn target_reads_tool_name_and_location() {
        let tool_call = ToolCallUpdate::new(
            "call-1",
            ToolCallUpdateFields::new()
                .name("ee_apply_patch")
                .kind(ToolKind::Edit)
                .locations(vec![ToolCallLocation::new("/work/src/lib.rs")]),
        );
        let target = PermissionTarget::from_tool_call(&tool_call);
        assert_eq!(target.action, "ee_apply_patch");
        assert_eq!(target.resource, "/work/src/lib.rs");
        assert_eq!(target.side_effect, PermissionSideEffect::Write);
    }

    #[test]
    fn target_falls_back_to_class_and_command() {
        let tool_call = ToolCallUpdate::new(
            "call-2",
            ToolCallUpdateFields::new()
                .kind(ToolKind::Execute)
                .raw_input(serde_json::json!({ "command": "cargo test" })),
        );
        let target = PermissionTarget::from_tool_call(&tool_call);
        assert_eq!(target.action, "execute");
        assert_eq!(target.resource, "cargo test");
        assert_eq!(target.side_effect, PermissionSideEffect::Execute);
    }

    #[test]
    fn saved_rules_are_persisted_and_reloaded() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("agent-permissions.json");
        let mut rules = PermissionRules::new(PermissionRuleset::empty()).with_persistence(&path);
        let allowed = target("bash", "cargo test", PermissionSideEffect::Execute);
        rules.remember_allow(&allowed);
        assert!(path.exists(), "saved approval written");

        let reloaded = PermissionRules::new(PermissionRuleset::empty()).with_persistence(&path);
        assert_eq!(reloaded.decide(&allowed), PermissionEffect::Allow);
    }

    #[test]
    fn malformed_saved_file_fails_closed() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("agent-permissions.json");
        fs::write(&path, "not json").expect("write");
        let rules = PermissionRules::new(PermissionRuleset::empty()).with_persistence(&path);
        assert!(rules.saved().is_empty());
    }
}
