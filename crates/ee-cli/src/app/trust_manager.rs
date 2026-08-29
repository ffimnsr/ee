//! Local `/permissions` trust manager and side-effect-free structured tester.

use std::collections::BTreeMap;

use super::App;
use super::agent_bridge::ActionLogEntry;
use crate::policy::evaluator::PolicyInput;
use crate::policy::manager::{
    RuleMutation, inspect_rule, mutate_rule, summarize_rules, test_policy,
};
use crate::policy::{
    BrowserActionClass, NetworkMethodClass, NetworkScheme, OperationIdentity, TraceStatus,
    TransportKind, TrustCategory, TrustOperation,
};

impl App {
    pub(super) fn agents_permissions_command(&mut self, args: &str) {
        let mut parts = args.splitn(2, char::is_whitespace);
        let action = parts.next().unwrap_or_default();
        let rest = parts.next().unwrap_or_default().trim();
        match action {
            "" => self.permissions_summary(),
            "list" if rest.is_empty() => self.permissions_list(),
            "inspect" if !rest.is_empty() => self.permissions_inspect(rest),
            "disable" if !rest.is_empty() => {
                self.permissions_mutate(rest, RuleMutation::Disable, "disable")
            }
            "enable" if !rest.is_empty() => {
                self.permissions_mutate(rest, RuleMutation::Enable, "enable")
            }
            "revoke" if !rest.is_empty() => {
                self.permissions_mutate(rest, RuleMutation::Revoke, "revoke")
            }
            "reload" if rest.is_empty() => self.permissions_reload(),
            "reset" if rest.is_empty() => self.permissions_notice(
                "Reset requires explicit confirmation. Run `/permissions reset confirm`; workspace rules/defaults removed, session choices unchanged.",
            ),
            "reset" if rest == "confirm" => self.permissions_reset(),
            "test" if !rest.is_empty() => self.permissions_test(rest, false),
            "preview" if !rest.is_empty() => self.permissions_test(rest, true),
            _ => self.permissions_notice(
                "usage: /permissions [list|inspect <id>|disable <id>|enable <id>|revoke <id>|reload|reset confirm|test <kind> field=value...|preview <kind> field=value...]",
            ),
        }
    }

    fn permissions_summary(&mut self) {
        let Some(store) = self.workspace_trust_store() else {
            self.permissions_notice("trust store unavailable; effective policy fails closed");
            return;
        };
        let managed = match store.load_for_management_at(self.trust_clock.now()) {
            Ok(document) => document,
            Err(error) => {
                self.permissions_notice(&format!("trust store unavailable: {error}"));
                return;
            }
        };
        let enabled = managed
            .document
            .rules
            .iter()
            .filter(|rule| managed.state(rule.id()).is_none_or(|state| state.enabled))
            .count();
        self.permissions_notice(&format!(
            "permissions workspace:{} persistent-rules:{}/{} tool-defaults:{} category-defaults:{} global-default:{:?} session-scope:memory-only store:{}",
            managed.document.workspace.as_string(),
            enabled,
            managed.document.rules.len(),
            managed.document.tool_defaults.len(),
            managed.document.category_defaults.len(),
            managed.document.global_default,
            store.path().display(),
        ));
    }

    fn permissions_list(&mut self) {
        let Some(store) = self.workspace_trust_store() else {
            self.permissions_notice("trust store unavailable; effective policy fails closed");
            return;
        };
        let managed = match store.load_for_management_at(self.trust_clock.now()) {
            Ok(document) => document,
            Err(error) => {
                self.permissions_notice(&format!("trust store unavailable: {error}"));
                return;
            }
        };
        let (session_id, _) = self.permission_session();
        let usage = self.agents.usage_ledger.snapshot(managed.document.workspace, &session_id);
        let mut lines = vec!["[built-in safeguards] application-owned, non-revocable".to_string()];
        let summaries = summarize_rules(&managed, &usage);
        for (effect, heading) in [
            (crate::policy::TrustEffect::Deny, "persistent deny"),
            (crate::policy::TrustEffect::Confirm, "mandatory confirm"),
            (crate::policy::TrustEffect::Allow, "bounded allow"),
        ] {
            lines.push(format!("[{heading}]"));
            lines.extend(
                summaries
                    .iter()
                    .filter(|summary| summary.effect == effect)
                    .map(|summary| summary.display()),
            );
        }
        lines.push("[defaults]".into());
        for default in &managed.document.tool_defaults {
            lines.push(format!("default tool:{} effect:{:?}", default.tool, default.effect));
        }
        for default in &managed.document.category_defaults {
            lines.push(format!(
                "default category:{} effect:{:?}",
                default.category.as_str(),
                default.effect
            ));
        }
        lines.push(format!("default global effect:{:?}", managed.document.global_default));
        self.permissions_notice(&lines.join("\n"));
    }

    fn permissions_inspect(&mut self, rule_id: &str) {
        let Some(store) = self.workspace_trust_store() else {
            self.permissions_notice("trust store unavailable; effective policy fails closed");
            return;
        };
        let managed = match store.load_for_management_at(self.trust_clock.now()) {
            Ok(document) => document,
            Err(error) => {
                self.permissions_notice(&format!("trust store unavailable: {error}"));
                return;
            }
        };
        let (session_id, _) = self.permission_session();
        let usage = self.agents.usage_ledger.snapshot(managed.document.workspace, &session_id);
        match inspect_rule(&managed, &usage, rule_id) {
            Some(summary) => self.permissions_notice(&summary.display()),
            None => self.permissions_notice("unknown or stale rule id"),
        }
    }

    fn permissions_mutate(&mut self, rule_id: &str, mutation: RuleMutation, action: &str) {
        let Some(store) = self.workspace_trust_store() else {
            self.permissions_notice("trust store unavailable; policy unchanged");
            return;
        };
        match mutate_rule(&store, rule_id, mutation, self.trust_clock.now()) {
            Ok(managed) => {
                self.agents.trust_policy.replace(Some(store.effective_at(self.trust_clock.now())));
                self.agents.action_log.push(ActionLogEntry::TrustRuleMutation {
                    rule_id: Some(rule_id.to_string()),
                    action: action.to_string(),
                    source: managed
                        .state(rule_id)
                        .map_or_else(|| "revoked".into(), |state| state.source.clone()),
                });
                self.permissions_notice(&format!(
                    "rule {rule_id} {action}d; durable policy active"
                ));
            }
            Err(error) => self.permissions_notice(&format!("rule unchanged: {error}")),
        }
    }

    fn permissions_reload(&mut self) {
        match self.reload_workspace_trust_store() {
            Ok(()) => {
                self.agents.action_log.push(ActionLogEntry::TrustRuleMutation {
                    rule_id: None,
                    action: "reload".into(),
                    source: "host-local-store".into(),
                });
                self.permissions_notice("host-local trust store reloaded")
            }
            Err(error) => {
                self.permissions_notice(&format!("reload failed; prior policy kept: {error}"))
            }
        }
    }

    fn permissions_reset(&mut self) {
        let Some(store) = self.workspace_trust_store() else {
            self.permissions_notice("trust store unavailable; policy unchanged");
            return;
        };
        match store.reset_at(self.trust_clock.now()) {
            Ok(_) => {
                self.agents.trust_policy.replace(Some(store.effective_at(self.trust_clock.now())));
                self.agents.action_log.push(ActionLogEntry::TrustRuleMutation {
                    rule_id: None,
                    action: "reset".into(),
                    source: "explicit-user-confirmation".into(),
                });
                self.permissions_notice(
                    "workspace persistent rules/defaults reset; session choices remain memory-only",
                );
            }
            Err(error) => {
                self.permissions_notice(&format!("reset failed; prior policy kept: {error}"))
            }
        }
    }

    fn permissions_test(&mut self, raw: &str, preview_only: bool) {
        let (session_id, agent) = self.permission_session();
        let operation = match parse_structured_operation(
            raw,
            self.primary_workspace_identity(),
            agent.as_deref(),
        ) {
            Ok(operation) => operation,
            Err(error) => {
                self.permissions_notice(&format!("validation failed: {error}"));
                return;
            }
        };
        if preview_only {
            self.permissions_notice(&format!(
                "candidate preview only; creates no authority: {}",
                normalized_identity(&operation)
            ));
            return;
        }
        let Some(store) = self.workspace_trust_store() else {
            self.permissions_notice("trust store unavailable; tester verdict: confirm");
            return;
        };
        let effective = store.effective_at(self.trust_clock.now());
        let usage = self.agents.usage_ledger.snapshot(operation.workspace, &session_id);
        let tool_key = operation.tool_key();
        let result = test_policy(&PolicyInput {
            session_id: &session_id,
            fingerprint: "permissions-structured-tester",
            operation: &operation,
            session: &self.agents.approval_policy,
            rules: &effective.rules,
            now: self.trust_clock.now(),
            usage: &usage,
            workspace_enabled: effective.workspace_enabled,
            built_in_deny: None,
            tool_default: effective
                .tool_defaults
                .iter()
                .find(|default| default.tool == tool_key)
                .map(|default| default.effect),
            category_default: effective
                .category_defaults
                .iter()
                .find(|default| default.category == operation.category)
                .map(|default| default.effect),
            global_default: Some(effective.global_default),
        });
        let trace = result
            .trace
            .iter()
            .map(|step| {
                let status = match step.status {
                    TraceStatus::NoMatch => "no-match",
                    TraceStatus::Matched => "matched",
                    TraceStatus::NotReached => "not-reached",
                };
                format!(
                    "{}={}{}",
                    step.layer,
                    status,
                    step.rule_id.as_ref().map_or_else(String::new, |id| format!(":{id}"))
                )
            })
            .collect::<Vec<_>>()
            .join(" > ");
        self.permissions_notice(&format!(
            "normalized:{}\nverdict:{:?} reason:{} matched:{}\nprecedence:{}\nside-effects:none",
            normalized_identity(&operation),
            result.decision.outcome,
            result.decision.reason.as_str(),
            result.decision.rule_id.as_deref().unwrap_or("fallback"),
            trace,
        ));
    }

    fn permission_session(&self) -> (String, Option<String>) {
        self.agents.active_thread.and_then(|index| self.agents.threads.get(index)).map_or_else(
            || ("permissions-manager".into(), None),
            |thread| (thread.session_id.clone(), Some(thread.agent_id.clone())),
        )
    }

    fn permissions_notice(&mut self, text: &str) {
        if let Some(index) = self.agents.active_thread
            && let Some(thread) = self.agents.threads.get_mut(index)
        {
            thread.push_system(text.to_string());
        }
        self.backend.status_message = Some(text.lines().next().unwrap_or(text).to_string());
    }
}

fn parse_structured_operation(
    raw: &str,
    workspace: crate::policy::WorkspaceIdentity,
    agent: Option<&str>,
) -> Result<TrustOperation, String> {
    let mut parts = raw.split_whitespace();
    let kind = parts.next().ok_or_else(|| "missing operation kind".to_string())?;
    let mut fields = BTreeMap::new();
    for part in parts {
        let (key, value) = part
            .split_once('=')
            .ok_or_else(|| format!("structured field must use key=value: {part}"))?;
        if key.is_empty() || value.is_empty() || fields.insert(key, value).is_some() {
            return Err(format!("invalid or duplicate structured field: {key}"));
        }
    }
    let operation = match kind {
        "command" => {
            require_only(&fields, &["executable", "argv"])?;
            let executable = required(&fields, "executable")?.to_string();
            let argv = csv(&fields, "argv")?;
            crate::policy::validate_command_tokens(&executable, &argv)?;
            TrustOperation {
                workspace,
                agent: agent.map(str::to_string),
                transport: TransportKind::Acp,
                category: TrustCategory::Execute,
                identity: OperationIdentity::Command { executable, argv },
            }
        }
        "path" => {
            require_only(&fields, &["path", "bytes"])?;
            let path = required(&fields, "path")?;
            let prefix = crate::policy::PathPrefix::parse(path)?;
            TrustOperation {
                workspace,
                agent: agent.map(str::to_string),
                transport: TransportKind::Acp,
                category: TrustCategory::Read,
                identity: OperationIdentity::ReadPath {
                    relative_path: prefix.display().to_string(),
                    byte_count: optional_u64(&fields, "bytes")?,
                },
            }
        }
        "write" => {
            require_only(&fields, &["path", "operation", "files", "bytes", "max_file_bytes"])?;
            let path = crate::policy::PathPrefix::parse(required(&fields, "path")?)?;
            let operation = match required(&fields, "operation")? {
                "create" => TrustCategory::WriteCreate,
                "modify" => TrustCategory::WriteModify,
                _ => return Err("write operation must be create or modify".into()),
            };
            TrustOperation {
                workspace,
                agent: agent.map(str::to_string),
                transport: TransportKind::Acp,
                category: operation,
                identity: OperationIdentity::Write {
                    relative_path: path.display().to_string(),
                    file_count: required_u64(&fields, "files")?,
                    total_bytes: optional_u64(&fields, "bytes")?,
                    max_file_bytes: optional_u64(&fields, "max_file_bytes")?,
                },
            }
        }
        "mcp" => {
            require_only(
                &fields,
                &["server", "transport", "tool", "schema", "arguments", "category"],
            )?;
            let category = parse_category(required(&fields, "category")?)?;
            let arguments = required(&fields, "arguments")?;
            let value: serde_json::Value = serde_json::from_str(arguments)
                .map_err(|error| format!("arguments must be JSON: {error}"))?;
            if !value.is_object() {
                return Err("arguments must be one JSON object".into());
            }
            TrustOperation {
                workspace,
                agent: agent.map(str::to_string),
                transport: TransportKind::McpStdio,
                category,
                identity: OperationIdentity::Mcp {
                    server: required(&fields, "server")?.to_string(),
                    transport_identity: required(&fields, "transport")?.to_string(),
                    tool: required(&fields, "tool")?.to_string(),
                    tool_schema_version: required_u64(&fields, "schema")?,
                    arguments_json: serde_json::to_string(&value)
                        .map_err(|error| format!("cannot normalize arguments: {error}"))?,
                },
            }
        }
        "network" => {
            require_only(&fields, &["scheme", "host", "port", "method", "action"])?;
            let scheme = match required(&fields, "scheme")? {
                "http" => NetworkScheme::Http,
                "https" => NetworkScheme::Https,
                "ws" => NetworkScheme::Ws,
                "wss" => NetworkScheme::Wss,
                _ => return Err("unknown network scheme".into()),
            };
            let method = match required(&fields, "method")? {
                "read" => NetworkMethodClass::Read,
                "write" => NetworkMethodClass::Write,
                "connect" => NetworkMethodClass::Connect,
                _ => return Err("unknown network method class".into()),
            };
            let action = match required(&fields, "action")? {
                "navigate" => BrowserActionClass::Navigate,
                "fetch" => BrowserActionClass::Fetch,
                "download" => BrowserActionClass::Download,
                "upload" => BrowserActionClass::Upload,
                "websocket" => BrowserActionClass::WebSocket,
                _ => return Err("unknown browser action class".into()),
            };
            TrustOperation {
                workspace,
                agent: agent.map(str::to_string),
                transport: TransportKind::Acp,
                category: TrustCategory::Network,
                identity: OperationIdentity::network(
                    scheme,
                    required(&fields, "host")?,
                    required_u64(&fields, "port")?.try_into().map_err(|_| "port must fit u16")?,
                    method,
                    action,
                )?,
            }
        }
        _ => return Err("kind must be command, path, write, mcp, or network".into()),
    };
    Ok(operation)
}

fn normalized_identity(operation: &TrustOperation) -> String {
    match &operation.identity {
        OperationIdentity::Command { argv, .. } => {
            format!("command exact argv-tokens:{}", argv.len())
        }
        OperationIdentity::ReadPath { byte_count, .. } => {
            format!("workspace read path-prefix segments; bytes:{byte_count:?}")
        }
        OperationIdentity::Write { file_count, total_bytes, max_file_bytes, .. } => format!(
            "workspace write path-prefix files:{file_count} total-bytes:{total_bytes:?} max-file-bytes:{max_file_bytes:?}"
        ),
        OperationIdentity::Mcp { tool_schema_version, .. } => {
            format!("MCP exact transport/tool/schema:{tool_schema_version}/arguments")
        }
        OperationIdentity::Network { scheme, port, method, browser_action, .. } => format!(
            "network exact host scheme:{scheme:?} port:{port} method:{method:?} action:{browser_action:?}"
        ),
        _ => "unsupported identity".into(),
    }
}

fn require_only(fields: &BTreeMap<&str, &str>, allowed: &[&str]) -> Result<(), String> {
    if let Some(key) = fields.keys().find(|key| !allowed.contains(key)) {
        return Err(format!("unknown structured field: {key}"));
    }
    Ok(())
}

fn required<'a>(fields: &'a BTreeMap<&str, &str>, key: &str) -> Result<&'a str, String> {
    fields.get(key).copied().ok_or_else(|| format!("missing {key}"))
}

fn csv(fields: &BTreeMap<&str, &str>, key: &str) -> Result<Vec<String>, String> {
    let value = required(fields, key)?;
    let values = value.split(',').map(str::to_string).collect::<Vec<_>>();
    if values.iter().any(String::is_empty) {
        return Err(format!("{key} contains empty token"));
    }
    Ok(values)
}

fn required_u64(fields: &BTreeMap<&str, &str>, key: &str) -> Result<u64, String> {
    required(fields, key)?.parse().map_err(|_| format!("{key} must be an unsigned integer"))
}

fn optional_u64(fields: &BTreeMap<&str, &str>, key: &str) -> Result<Option<u64>, String> {
    fields
        .get(key)
        .map(|value| value.parse().map_err(|_| format!("{key} must be an unsigned integer")))
        .transpose()
}

fn parse_category(raw: &str) -> Result<TrustCategory, String> {
    match raw {
        "read" => Ok(TrustCategory::Read),
        "write_create" => Ok(TrustCategory::WriteCreate),
        "write_modify" => Ok(TrustCategory::WriteModify),
        "delete" => Ok(TrustCategory::Delete),
        "execute" => Ok(TrustCategory::Execute),
        "network" => Ok(TrustCategory::Network),
        _ => Err("unknown side-effect category".into()),
    }
}
