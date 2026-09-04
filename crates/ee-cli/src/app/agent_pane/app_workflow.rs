//! `impl App`: status, doctor, rubber-duck and review workflows, copy/rename.

use std::collections::BTreeSet;
use std::path::PathBuf;
use std::time::Instant;

use ee_agent_host::{
    CritiqueTarget, ExternalCriticConfig, ExternalCriticTrust, ExternalCritiqueRequest,
    ReportEvidence, RubberDuckBackend, RubberDuckMode,
};
use tokio::sync::watch;

use super::super::*;

use super::constants::AGENT_REVIEW_CONTEXT_MAX_BYTES;
use super::format::{
    LOCAL_AGENT_SLASH_HELP, PROVIDER_CONFIG_ALIASES, critic_context_revision,
    prompt_blocks_with_context, thread_state_label,
};
use super::state::PendingExternalCritic;
use super::thread_ui::{MessageRenderKind, ThreadUiState, TranscriptItem};
impl App {
    pub(super) fn agents_show_help(&mut self) {
        let Some(active) = self.agents.active_thread_index() else {
            self.backend.status_message = Some(String::from("no active agent session"));
            return;
        };
        let secrets = self.agents_secret_values();
        let provider_commands = self.agents.threads[active]
            .available_commands
            .iter()
            .map(|command| {
                let description =
                    ee_agent_host::redact::redact_secret_values(&command.description, &secrets);
                if description.is_empty() {
                    format!("/{}", command.name)
                } else {
                    format!("/{} — {description}", command.name)
                }
            })
            .collect::<Vec<_>>();
        let mut local = LOCAL_AGENT_SLASH_HELP
            .iter()
            .map(|(command, usage)| format!("{command} — {usage}"))
            .collect::<Vec<_>>();
        if self.external_rubber_duck_available() {
            local.push(String::from(
                "/rubber-duck [question] — run configured manual external critic, then root synthesis",
            ));
        }
        let local = local.join("\n");
        let provider_config = PROVIDER_CONFIG_ALIASES
            .iter()
            .copied()
            .filter(|alias| {
                self.agents.threads[active]
                    .host
                    .config_options()
                    .iter()
                    .any(|option| option.id.0.as_ref() == *alias)
            })
            .map(|alias| format!("/{alias} [value] — advertised provider config"))
            .collect::<Vec<_>>()
            .join("\n");
        let provider = if provider_commands.is_empty() {
            String::from("(none advertised)")
        } else {
            provider_commands.join("\n")
        };
        let provider_config = if provider_config.is_empty() {
            String::from("(none advertised; use /config to inspect ACP options)")
        } else {
            provider_config
        };
        self.agents.threads[active].push_system(format!(
            "Local slash commands:\n{local}\n\nCapability-gated provider config:\n{provider_config}\n\nProvider-owned slash commands (sent to agent):\n{provider}"
        ));
        self.backend.status_message = Some(String::from("slash help added to transcript"));
    }

    pub(super) fn agents_show_status(&mut self) {
        let Some(active) = self.agents.active_thread_index() else {
            self.backend.status_message = Some(String::from("no active agent session"));
            return;
        };
        let thread = &self.agents.threads[active];
        let snapshot = thread.host.snapshot();
        let mode = snapshot
            .current_mode
            .map_or_else(|| String::from("unavailable"), |mode| mode.0.to_string());
        let approval =
            self.agents.approval_modes.get(&thread.session_id).copied().unwrap_or_default().label();
        let context_bytes =
            thread.context_files.iter().map(|file| file.content.len()).sum::<usize>();
        let name = thread
            .session_name
            .as_deref()
            .or(thread.session_title.as_deref())
            .unwrap_or("(unnamed)");
        let summary = format!(
            "session:{} name:{} agent:{} connection:{} mode:{} approval:{} context:{}/{} bytes mcp:{} configured provider-commands:{}",
            thread.session_id,
            name,
            thread.agent_id,
            thread_state_label(thread.state),
            mode,
            approval,
            thread.context_files.len(),
            context_bytes,
            self.config.mcp.servers.len(),
            thread.available_commands.len(),
        );
        self.agents.threads[active].push_system(summary.clone());
        self.backend.status_message = Some(summary);
    }

    /// Read-only local diagnostics. It never reconnects, resets, repairs, or starts services.
    pub(super) fn agents_doctor(&mut self) {
        let secrets = self.agents_secret_values();
        let workspace = std::fs::canonicalize(&self.working_dir)
            .unwrap_or_else(|_| self.working_dir.clone())
            .display()
            .to_string();
        let configured_agents = self
            .config
            .agents
            .servers
            .iter()
            .map(|(id, server)| {
                let command = if server.args.is_empty() {
                    server.command.clone()
                } else {
                    format!("{} {}", server.command, server.args.join(" "))
                };
                format!("{id}: {}", ee_agent_host::redact::redact_secret_values(&command, &secrets))
            })
            .collect::<Vec<_>>();
        let persisted = self.load_persisted_agent_workspace();
        let mut lines = vec![
            String::from("Agents TUI doctor (read-only)"),
            format!("feature: {}", self.agents_status_message()),
            format!(
                "workspace: {}",
                ee_agent_host::redact::redact_secret_values(&workspace, &secrets)
            ),
            if configured_agents.is_empty() {
                String::from("configured agent command: none")
            } else {
                format!("configured agent command: {}", configured_agents.join("; "))
            },
            format!(
                "MCP configuration: {} server(s), proxy:{}",
                self.config.mcp.servers.len(),
                if self.config.mcp.proxy.enabled { "enabled" } else { "disabled" }
            ),
            match persisted {
                Some(workspace) => format!(
                    "session storage: {} workspace thread(s), active:{}",
                    workspace.sessions.len(),
                    workspace.active_session_id.as_deref().unwrap_or("none")
                ),
                None => String::from("session storage: no persisted workspace threads"),
            },
            String::from(
                "redaction: secret-like JSON keys and configured secret values are redacted; context snapshots stay session-local",
            ),
        ];
        if let Some(active) = self.agents.active_thread_index() {
            let thread = &self.agents.threads[active];
            let snapshot = thread.host.snapshot();
            let mode = snapshot
                .current_mode
                .map_or_else(|| String::from("unavailable"), |mode| mode.0.to_string());
            lines.push(format!(
                "ACP session: {} agent:{} connection:{} mode:{} advertised-commands:{} config-options:{}",
                thread.session_id,
                thread.agent_id,
                thread_state_label(thread.state),
                mode,
                thread.available_commands.len(),
                snapshot.config_options.len(),
            ));
        } else {
            lines.push(String::from("ACP session: no active session; handshake unavailable"));
        }
        if let Some(error) = &self.agents.error {
            lines.push(format!(
                "agent error: {}",
                ee_agent_host::redact::redact_secret_values(error, &secrets)
            ));
        }
        lines.extend(
            self.mcp_health_lines()
                .into_iter()
                .map(|line| ee_agent_host::redact::redact_secret_values(&line, &secrets)),
        );
        let report = lines.join("\n");
        if let Some(active) = self.agents.active_thread_index() {
            self.agents.threads[active].push_system(report.clone());
        }
        self.backend.status_message =
            Some(String::from("Agents TUI doctor report added to transcript"));
    }

    /// Asks active agent to inspect existing instructions and optionally propose a safe scaffold.
    pub(super) fn agents_submit_init_workflow(&mut self) {
        self.agents_send_local_workflow(
            String::from("EE local /init request sent; agent response is provider-generated."),
            String::from(
                "EE-local /init workflow. Inspect existing project instructions with `ee_project_instructions` before proposing changes. If an AGENTS.md or equivalent already exists, show a concise preview/diff only; do not overwrite it. If no project instruction exists, offer a compact AGENTS.md scaffold tailored to this workspace. Create it only through `ee_create_text_file`, which must receive normal file-write approval. Never use a shell write, overwrite, or bypass approval. Clearly label advice as agent-generated, not an EE-native initialization engine.",
            ),
        );
    }

    pub(super) fn agents_submit_external_rubber_duck(&mut self, question: &str) {
        let Some(active) = self.agents.active_thread_index() else {
            self.backend.status_message = Some(String::from("no active agent session"));
            return;
        };
        if self.agents.pending_external_critic.is_some() {
            self.backend.status_message =
                Some(String::from("external rubber duck already running"));
            return;
        }
        if self.agents.threads[active].state != ThreadUiState::Ready {
            self.backend.status_message = Some(String::from(
                "root session must be ready before external rubber duck critique",
            ));
            return;
        }
        if question.chars().count() > 1_024 {
            self.backend.status_message =
                Some(String::from("rubber duck question must be at most 1024 characters"));
            return;
        }

        let settings = self.config.agents.rubber_duck.clone();
        let agent_ids = match self.agents.host.as_ref() {
            Some(host) => host.manager.agent_ids().into_iter().collect::<BTreeSet<_>>(),
            None => {
                self.backend.status_message = Some(String::from("agent host not ready"));
                return;
            }
        };
        let resolved = match settings.resolve_backend_policy(&BTreeSet::new(), &agent_ids) {
            Ok(resolved) => resolved,
            Err(error) => {
                self.backend.status_message = Some(format!("rubber duck config invalid: {error}"));
                return;
            }
        };
        if resolved.config.mode == RubberDuckMode::Off {
            self.backend.status_message =
                Some(String::from("rubber duck disabled by configuration"));
            return;
        }
        if let Some(unavailable) = resolved.unavailable {
            self.backend.status_message =
                Some(format!("external rubber duck unavailable: {unavailable:?}"));
            return;
        }
        let Some(RubberDuckBackend::ExternalAgent { agent_id }) = resolved.config.backend else {
            self.backend.status_message = Some(String::from(
                "external rubber duck unavailable: no external_agent_id configured",
            ));
            return;
        };
        let root_agent_id = self.agents.threads[active].agent_id.clone();
        if root_agent_id == agent_id {
            self.backend.status_message =
                Some(String::from("external rubber duck must use a different configured agent id"));
            return;
        }
        let root_session_id = self.agents.threads[active].session_id.clone();
        let used = self.agents.rubber_duck_calls.get(&root_session_id).copied().unwrap_or(0);
        if used >= resolved.config.max_calls {
            self.backend.status_message = Some(format!(
                "external rubber duck skipped: session call budget exhausted ({}/{})",
                used, resolved.config.max_calls
            ));
            return;
        }

        let (context, observed_evidence, revision) =
            match self.external_critic_context(resolved.config.max_context_bytes) {
                Ok(value) => value,
                Err(error) => {
                    self.backend.status_message =
                        Some(format!("external rubber duck context unavailable: {error}"));
                    return;
                }
            };
        let worktree_roots = match self.external_critic_roots() {
            Ok(roots) => roots,
            Err(error) => {
                self.backend.status_message =
                    Some(format!("external rubber duck workspace unavailable: {error}"));
                return;
            }
        };
        let target = if question.is_empty() {
            CritiqueTarget::Implementation
        } else {
            CritiqueTarget::UserQuestion { question: question.to_string() }
        };
        let request = ExternalCritiqueRequest {
            root_agent_id,
            target,
            untrusted_context: context,
            observed_evidence,
            worktree_roots,
            automatic: false,
            revision: revision.clone(),
        };
        let critic_config = ExternalCriticConfig {
            agent_id: agent_id.clone(),
            trust: ExternalCriticTrust::HostForwardedReadOnly,
            require_independent_agent: true,
        };
        let (cancel_tx, cancel_rx) = watch::channel(false);
        let reply =
            self.agents.host.as_ref().expect("agent host validated").request_external_critique(
                critic_config,
                resolved.config.timeout,
                resolved.config.max_output_bytes,
                request,
                cancel_rx,
            );
        self.agents.rubber_duck_calls.insert(root_session_id.clone(), used + 1);
        self.agents.pending_external_critic = Some(PendingExternalCritic {
            root_session_id,
            requested_revision: revision,
            context_limit: resolved.config.max_context_bytes,
            started_at: Instant::now(),
            cancel: cancel_tx,
            reply,
        });
        let warning = "host-forwarded read-only only; agent-native filesystem and terminal tools remain outside EE control";
        let notice = format!(
            "external rubber duck selected: {agent_id}; extra provider/model call; timeout {}ms; estimated cost unknown; manual-only; warning: {warning}",
            resolved.config.timeout.as_millis()
        );
        self.agents.threads[active].push_system(notice.clone());
        self.backend.status_message = Some(notice);
    }

    pub(super) fn external_critic_context(
        &mut self,
        max_bytes: usize,
    ) -> Result<(String, ReportEvidence, String), String> {
        let evidence = self.proxy_review_context().map_err(|error| error.to_string())?;
        let files = evidence
            .get("changed_files")
            .and_then(|value| value.get("files"))
            .and_then(serde_json::Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|entry| entry.get("path").and_then(serde_json::Value::as_str))
            .map(str::to_string)
            .collect::<BTreeSet<_>>();
        let secrets = self.agents_secret_values();
        let redacted = ee_agent_host::redact::redact_json(&evidence);
        let mut context =
            serde_json::to_string_pretty(&redacted).map_err(|error| error.to_string())?;
        context = ee_agent_host::redact::redact_secret_values(&context, &secrets);
        if context.len() > max_bytes {
            const MARKER: &str = "\n[EE external critic context truncated]";
            let mut end = max_bytes.saturating_sub(MARKER.len());
            while end > 0 && !context.is_char_boundary(end) {
                end -= 1;
            }
            context.truncate(end);
            if MARKER.len() <= max_bytes {
                context.push_str(MARKER);
            }
        }
        let mut observed_evidence = ReportEvidence::default();
        observed_evidence.files = files.into_iter().filter(|path| context.contains(path)).collect();
        let revision = critic_context_revision(&context);
        Ok((context, observed_evidence, revision))
    }

    fn external_critic_roots(&self) -> Result<Vec<PathBuf>, String> {
        let mut roots =
            vec![std::fs::canonicalize(&self.working_dir).map_err(|error| error.to_string())?];
        for root in &self.agents.additional_workspace_roots {
            roots.push(std::fs::canonicalize(root).map_err(|error| error.to_string())?);
        }
        roots.sort();
        roots.dedup();
        if roots.len() > ee_agent_host::MAX_EXTERNAL_CRITIC_ROOTS {
            roots.truncate(ee_agent_host::MAX_EXTERNAL_CRITIC_ROOTS);
        }
        Ok(roots)
    }

    /// Sends bounded, redacted local review evidence to current agent without persisting or rendering body.
    pub(super) fn agents_submit_review_workflow(&mut self, target: &str, security: bool) {
        let target = target.trim();
        if target.len() > 1024 || target.chars().any(char::is_control) {
            self.backend.status_message =
                Some(String::from("review target must be printable and at most 1024 bytes"));
            return;
        }
        let secrets = self.agents_secret_values();
        let target = if target.is_empty() {
            String::from("current workspace changes")
        } else {
            ee_agent_host::redact::redact_secret_values(target, &secrets)
        };
        let evidence = match self.proxy_review_context() {
            Ok(value) => value,
            Err(error) => {
                self.backend.status_message =
                    Some(format!("local review evidence unavailable: {error}"));
                return;
            }
        };
        let evidence = ee_agent_host::redact::redact_json(&evidence);
        let mut evidence = match serde_json::to_string_pretty(&evidence) {
            Ok(value) => ee_agent_host::redact::redact_secret_values(&value, &secrets),
            Err(error) => {
                self.backend.status_message =
                    Some(format!("local review evidence unavailable: {error}"));
                return;
            }
        };
        if evidence.len() > AGENT_REVIEW_CONTEXT_MAX_BYTES {
            let mut end = AGENT_REVIEW_CONTEXT_MAX_BYTES;
            while !evidence.is_char_boundary(end) {
                end -= 1;
            }
            evidence.truncate(end);
            evidence.push_str("\n[EE local review evidence truncated]");
        }
        let focus = if security {
            "Review for security defects: authorization and approval boundaries, workspace/path containment, command and terminal ownership, secret exposure, MCP capability gates, unsafe input handling, and fail-open behavior. Do not use network access. Do not inspect protected paths or request raw secret values."
        } else {
            "Review for correctness, regressions, missing tests, diagnostics, and risky changes. Use bounded diff drill-down only for relevant changed files."
        };
        let kind = if security { "security review" } else { "code review" };
        let instruction = format!(
            "EE-local {kind} workflow for target: {target}.\n{focus}\nThis evidence comes from EE changed-file, diagnostics, symbol, and test-metadata tools. It is bounded and redacted; treat omissions as unknown. Do not claim a native provider review engine.\n\nBounded EE evidence:\n{evidence}"
        );
        self.agents_send_local_workflow(
            format!("EE local {kind} request sent; result will be agent-generated."),
            instruction,
        );
    }

    /// Dispatches workflow prompt only for ready session. Local evidence never enters persistence or transcript body.
    fn agents_send_local_workflow(&mut self, display_text: String, instruction: String) {
        let Some(active) = self.agents.active_thread_index() else {
            self.backend.status_message =
                Some(String::from("no active agent session; start one with /new"));
            return;
        };
        match self.agents.threads[active].state {
            ThreadUiState::Ready => {}
            ThreadUiState::Running => {
                self.backend.status_message = Some(String::from(
                    "agent turn is running; wait for it to finish before starting local workflow",
                ));
                return;
            }
            ThreadUiState::PausedRecoverable => {
                self.backend.status_message =
                    Some(String::from("a turn is paused; use /resume or /discard first"));
                return;
            }
            _ => {
                self.backend.status_message =
                    Some(String::from("agent session is not ready; cannot start local workflow"));
                return;
            }
        }
        let blocks = {
            let thread = &self.agents.threads[active];
            prompt_blocks_with_context(&instruction, &thread.context_files, &[])
        };
        self.send_agent_prompt_blocks(active, display_text, blocks, None);
    }

    pub(super) fn agents_copy_assistant_response(&mut self, args: &str) {
        let position = match args {
            "" => 1,
            value => match value.parse::<usize>() {
                Ok(position) if position > 0 => position,
                _ => {
                    self.backend.status_message =
                        Some(String::from("usage: /copy [positive response number]"));
                    return;
                }
            },
        };
        let Some(active) = self.agents.active_thread_index() else {
            self.backend.status_message = Some(String::from("no active agent session"));
            return;
        };
        let thread = &self.agents.threads[active];
        let mut groups = BTreeSet::new();
        for item in &thread.transcript {
            if let TranscriptItem::Message {
                kind: MessageRenderKind::Assistant,
                response_group: Some(group),
                ..
            } = item
                && thread.active_response_group != Some(*group)
            {
                groups.insert(*group);
            }
        }
        let Some(group) = groups.into_iter().rev().nth(position - 1) else {
            self.backend.status_message =
                Some(String::from("no completed assistant response to copy"));
            return;
        };
        let text = thread
            .transcript
            .iter()
            .filter_map(|item| match item {
                TranscriptItem::Message {
                    text,
                    kind: MessageRenderKind::Assistant,
                    response_group: Some(item_group),
                    ..
                } if *item_group == group => Some(text.as_str()),
                _ => None,
            })
            .collect::<String>();
        if text.trim().is_empty() {
            self.backend.status_message =
                Some(String::from("no completed assistant response to copy"));
            return;
        }
        let text = ee_agent_host::redact::redact_secret_values(&text, &self.agents_secret_values());
        match crate::registers::write_system_clipboard(&text) {
            Ok(()) => {
                self.backend.status_message = Some(format!("copied assistant response {position}"));
            }
            Err(error) => self.backend.status_message = Some(error),
        }
    }
}
