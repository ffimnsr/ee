//! `impl App`: context-file and workspace-memory commands.

use std::io::Read;
use std::path::{Path, PathBuf};

use super::super::*;

use super::constants::{
    AGENT_ADDITIONAL_ROOT_MAX, AGENT_CONTEXT_MAX_FILE_BYTES, AGENT_CONTEXT_MAX_FILES,
    AGENT_CONTEXT_MAX_TOTAL_BYTES,
};
use super::format::{sanitize_session_name, thread_display_name};
use super::state::AdditionalDirectoryConfirmation;
use super::thread_ui::AgentContextFile;
impl App {
    pub(super) fn agents_rename_session(&mut self, raw_name: &str) {
        let Some(name) = sanitize_session_name(raw_name) else {
            self.backend.status_message =
                Some(String::from("session name must contain visible text"));
            return;
        };
        let Some(active) = self.agents.active_thread_index() else {
            self.backend.status_message = Some(String::from("no active agent session"));
            return;
        };
        let thread = &mut self.agents.threads[active];
        thread.session_name = Some(name.clone());
        thread.display_name = thread_display_name(
            thread.index,
            &thread.agent_id,
            thread.session_name.as_deref(),
            thread.session_title.as_deref(),
        );
        thread.push_system(format!("session renamed: {name}"));
        self.persist_agent_workspace();
        self.backend.status_message = Some(format!("session renamed: {name}"));
    }

    fn agents_list_context_files(&mut self) {
        let Some(active) = self.agents.active_thread_index() else {
            self.backend.status_message = Some(String::from("no active agent session"));
            return;
        };
        let files = &self.agents.threads[active].context_files;
        self.backend.status_message = Some(if files.is_empty() {
            String::from("context files: none")
        } else {
            format!(
                "context files: {}",
                files
                    .iter()
                    .map(|file| format!("{} ({} bytes)", file.relative_path, file.content.len()))
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        });
    }

    fn agents_add_context_file(&mut self, path: &str) {
        let Some(active) = self.agents.active_thread_index() else {
            self.backend.status_message = Some(String::from("no active agent session"));
            return;
        };
        let (canonical, relative_path, content) =
            match self.agent_context_file_snapshot(Path::new(path), AGENT_CONTEXT_MAX_FILE_BYTES) {
                Ok(snapshot) => snapshot,
                Err(error) => {
                    self.backend.status_message = Some(error);
                    return;
                }
            };
        let content_len = content.len();
        let thread = &mut self.agents.threads[active];
        let existing = thread.context_files.iter().position(|file| file.path == canonical);
        let total = thread
            .context_files
            .iter()
            .enumerate()
            .filter(|(index, _)| Some(*index) != existing)
            .map(|(_, file)| file.content.len())
            .sum::<usize>();
        if total.saturating_add(content_len) > AGENT_CONTEXT_MAX_TOTAL_BYTES {
            self.backend.status_message = Some(format!(
                "context files exceed {AGENT_CONTEXT_MAX_TOTAL_BYTES} byte total limit"
            ));
            return;
        }
        let attached_path = relative_path.clone();
        let context_file = AgentContextFile { path: canonical, relative_path, content };
        if let Some(existing) = existing {
            thread.context_files[existing] = context_file;
        } else {
            if thread.context_files.len() >= AGENT_CONTEXT_MAX_FILES {
                self.backend.status_message =
                    Some(format!("context file limit reached ({AGENT_CONTEXT_MAX_FILES})"));
                return;
            }
            thread.context_files.push(context_file);
        }
        let notice = format!("context attached: {attached_path} ({content_len} bytes)");
        thread.push_system(notice.clone());
        self.backend.status_message = Some(notice);
    }

    fn agents_remove_context_file(&mut self, path: &str) {
        let Some(active) = self.agents.active_thread_index() else {
            self.backend.status_message = Some(String::from("no active agent session"));
            return;
        };
        let thread = &mut self.agents.threads[active];
        let Some(index) = thread.context_files.iter().position(|file| file.relative_path == path)
        else {
            self.backend.status_message = Some(format!("context file not attached: {path}"));
            return;
        };
        let removed = thread.context_files.remove(index);
        let notice = format!("context removed: {}", removed.relative_path);
        thread.push_system(notice.clone());
        self.backend.status_message = Some(notice);
    }

    fn agents_clear_context_files(&mut self) {
        let Some(active) = self.agents.active_thread_index() else {
            self.backend.status_message = Some(String::from("no active agent session"));
            return;
        };
        let thread = &mut self.agents.threads[active];
        let count = thread.context_files.len();
        thread.context_files.clear();
        let notice = format!("context files cleared ({count})");
        thread.push_system(notice.clone());
        self.backend.status_message = Some(notice);
    }

    fn agents_context_status(&mut self) {
        let Some(active) = self.agents.active_thread_index() else {
            self.backend.status_message = Some(String::from("no active agent session"));
            return;
        };
        let thread = &self.agents.threads[active];
        let session_bytes =
            thread.context_files.iter().map(|file| file.content.len()).sum::<usize>();
        let mention_bytes =
            thread.next_prompt_context_files.iter().map(|file| file.content.len()).sum::<usize>();
        let paths = thread
            .context_files
            .iter()
            .map(|file| format!("{} ({} bytes)", file.relative_path, file.content.len()))
            .collect::<Vec<_>>();
        let mentions = thread
            .next_prompt_context_files
            .iter()
            .map(|file| format!("{} ({} bytes)", file.relative_path, file.content.len()))
            .collect::<Vec<_>>();
        let summary = format!(
            "context scope=session-only; selected:[{}]; one-turn mentions:[{}]; totals:{} session + {} one-turn / {} bytes; caps:{} files, {} bytes/file, {} bytes total",
            if paths.is_empty() { String::from("none") } else { paths.join(", ") },
            if mentions.is_empty() { String::from("none") } else { mentions.join(", ") },
            session_bytes,
            mention_bytes,
            AGENT_CONTEXT_MAX_TOTAL_BYTES,
            AGENT_CONTEXT_MAX_FILES,
            AGENT_CONTEXT_MAX_FILE_BYTES,
            AGENT_CONTEXT_MAX_TOTAL_BYTES,
        );
        self.agents.threads[active].push_system(summary.clone());
        self.backend.status_message = Some(summary);
    }

    /// Adds a bounded, redacted snapshot to exactly the next submitted prompt.
    pub(super) fn agents_mention_context_file(&mut self, path: &str) {
        let Some(active) = self.agents.active_thread_index() else {
            self.backend.status_message = Some(String::from("no active agent session"));
            return;
        };
        let (canonical, relative_path, content) =
            match self.agent_context_file_snapshot(Path::new(path), AGENT_CONTEXT_MAX_FILE_BYTES) {
                Ok(snapshot) => snapshot,
                Err(error) => {
                    self.backend.status_message = Some(error);
                    return;
                }
            };
        let content_len = content.len();
        let thread = &mut self.agents.threads[active];
        let existing =
            thread.next_prompt_context_files.iter().position(|file| file.path == canonical);
        let total = thread
            .context_files
            .iter()
            .chain(thread.next_prompt_context_files.iter())
            .enumerate()
            .filter(|(index, _)| {
                *index < thread.context_files.len()
                    || Some(*index - thread.context_files.len()) != existing
            })
            .map(|(_, file)| file.content.len())
            .sum::<usize>();
        if total.saturating_add(content_len) > AGENT_CONTEXT_MAX_TOTAL_BYTES {
            self.backend.status_message = Some(format!(
                "context snapshots exceed {AGENT_CONTEXT_MAX_TOTAL_BYTES} byte total limit"
            ));
            return;
        }
        if existing.is_none()
            && thread.context_files.len() + thread.next_prompt_context_files.len()
                >= AGENT_CONTEXT_MAX_FILES
        {
            self.backend.status_message =
                Some(format!("context file limit reached ({AGENT_CONTEXT_MAX_FILES})"));
            return;
        }
        let mention =
            AgentContextFile { path: canonical, relative_path: relative_path.clone(), content };
        if let Some(existing) = existing {
            thread.next_prompt_context_files[existing] = mention;
        } else {
            thread.next_prompt_context_files.push(mention);
        }
        let notice =
            format!("mention attached for next prompt only: {relative_path} ({content_len} bytes)");
        thread.push_system(notice.clone());
        self.backend.status_message = Some(notice);
    }

    pub(super) fn request_additional_workspace_directory(&mut self, raw_path: &str) {
        let Some(active) = self.agents.active_thread_index() else {
            self.backend.status_message = Some(String::from("no active agent session"));
            return;
        };
        if !self.agents.threads[active].host.supports_additional_directories() {
            self.backend.status_message =
                Some(String::from("agent does not advertise additional-directory capability"));
            return;
        }
        let path = Path::new(raw_path);
        let candidate =
            if path.is_absolute() { path.to_path_buf() } else { self.working_dir.join(path) };
        let canonical = match std::fs::canonicalize(&candidate) {
            Ok(path) => path,
            Err(error) => {
                self.backend.status_message = Some(format!(
                    "cannot access additional directory {}: {error}",
                    candidate.display()
                ));
                return;
            }
        };
        if !canonical.is_dir() {
            self.backend.status_message =
                Some(format!("additional root is not a directory: {}", canonical.display()));
            return;
        }
        if self.is_secret_store_path(&canonical) {
            self.backend.status_message = Some(String::from("additional root is protected"));
            return;
        }
        if canonical
            == std::fs::canonicalize(&self.working_dir).unwrap_or_else(|_| self.working_dir.clone())
        {
            self.backend.status_message =
                Some(String::from("directory is already primary workspace root"));
            return;
        }
        if self.agents.additional_workspace_roots.contains(&canonical) {
            self.backend.status_message = Some(format!(
                "additional root already trusted for this session: {}",
                canonical.display()
            ));
            return;
        }
        if self.agents.additional_workspace_roots.len() >= AGENT_ADDITIONAL_ROOT_MAX {
            self.backend.status_message =
                Some(format!("additional root limit reached ({AGENT_ADDITIONAL_ROOT_MAX})"));
            return;
        }
        self.agents.additional_directory_confirmation =
            Some(AdditionalDirectoryConfirmation { path: canonical.clone() });
        self.backend.status_message =
            Some(format!("confirm additional workspace root: {}", canonical.display()));
    }

    pub(super) fn confirm_additional_workspace_directory(&mut self) {
        let Some(confirmation) = self.agents.additional_directory_confirmation.take() else {
            return;
        };
        self.agents.additional_workspace_roots.insert(confirmation.path.clone());
        self.backend.status_message = Some(format!(
            "additional root trusted for this Agents TUI session: {}; current provider session unchanged; /new uses it when supported",
            confirmation.path.display()
        ));
    }

    fn push_workspace_memory_system(&mut self, mut text: String) {
        const MAX_MESSAGE_BYTES: usize = 32 * 1024;
        if text.len() > MAX_MESSAGE_BYTES {
            let mut end = MAX_MESSAGE_BYTES;
            while !text.is_char_boundary(end) {
                end -= 1;
            }
            text.truncate(end);
            text.push_str("\n[workspace memory output truncated]");
        }
        if let Some(index) = self.agents.active_thread_index() {
            self.agents.threads[index].push_system(text);
        } else {
            self.backend.status_message = Some(text);
        }
    }

    fn render_workspace_fact(fact: &ee_mcp::WorkspaceFact) -> String {
        const MAX_VALUE_BYTES: usize = 2 * 1024;
        let mut value = fact.value.clone();
        if value.len() > MAX_VALUE_BYTES {
            let mut end = MAX_VALUE_BYTES;
            while !value.is_char_boundary(end) {
                end -= 1;
            }
            value.truncate(end);
            value.push('…');
        }
        format!(
            "key: {}\nvalue: {}\nauthority: {} · freshness: {} · state: {}\nsource: {}:{} · verified: {} · selection: {}",
            fact.key,
            value,
            fact.authority,
            fact.freshness,
            fact.state,
            fact.provenance.source_kind,
            fact.provenance.source_id,
            fact.provenance.verified_at.as_deref().unwrap_or("unverified"),
            fact.selection_reason.as_deref().unwrap_or("exact read")
        )
    }

    fn render_workspace_facts(result: &ee_mcp::WorkspaceFactsResult) -> String {
        let mut text = format!(
            "workspace memory: {} shown · {} total · {} omitted · truncated: {}",
            result.facts.len(),
            result.total,
            result.omitted,
            result.truncated
        );
        for fact in &result.facts {
            text.push_str("\n\n");
            text.push_str(&Self::render_workspace_fact(fact));
        }
        text
    }

    fn read_workspace_memory_import(
        &self,
        argument: &str,
    ) -> Result<(PathBuf, ee_agent_host::WorkspaceMemoryExportDto), String> {
        const MAX_IMPORT_BYTES: u64 = 16 * 1024 * 1024;
        let requested = PathBuf::from(argument);
        let path =
            if requested.is_absolute() { requested } else { self.working_dir.join(requested) };
        let metadata = std::fs::symlink_metadata(&path)
            .map_err(|error| format!("cannot inspect {}: {error}", path.display()))?;
        if metadata.file_type().is_symlink() || !metadata.is_file() {
            return Err(format!(
                "import path must be a regular non-symlink file: {}",
                path.display()
            ));
        }
        if metadata.len() > MAX_IMPORT_BYTES {
            return Err(format!(
                "workspace memory import exceeds {MAX_IMPORT_BYTES} bytes: {}",
                path.display()
            ));
        }
        let file = std::fs::File::open(&path)
            .map_err(|error| format!("cannot open {}: {error}", path.display()))?;
        let mut bytes = Vec::with_capacity(metadata.len() as usize);
        file.take(MAX_IMPORT_BYTES + 1)
            .read_to_end(&mut bytes)
            .map_err(|error| format!("cannot read {}: {error}", path.display()))?;
        if bytes.len() as u64 > MAX_IMPORT_BYTES {
            return Err(format!(
                "workspace memory import exceeds {MAX_IMPORT_BYTES} bytes: {}",
                path.display()
            ));
        }
        let export = serde_json::from_slice(&bytes).map_err(|error| {
            format!("invalid workspace memory export {}: {error}", path.display())
        })?;
        Ok((path, export))
    }

    fn workspace_memory_config_path(&self) -> PathBuf {
        self.working_dir.join(".ee.toml")
    }

    fn persist_workspace_memory_switch(&mut self, enabled: bool) {
        let path = self.workspace_memory_config_path();
        if let Err(error) = crate::config::persist_workspace_memory_enabled(&path, enabled) {
            self.backend.status_message =
                Some(format!("workspace memory config update failed: {error}"));
            return;
        }
        let host_was_initialized = self.agents.host.is_some();
        self.config.agents.workspace_memory.enabled = enabled;
        let state = if enabled { "enabled" } else { "disabled" };
        let data = if enabled {
            "Existing local facts become available after backend activation."
        } else {
            "Local database and facts were kept. Use `/memory disable --delete` while memory is active to clear this canonical workspace first."
        };
        let activation = if host_was_initialized {
            " Running agent host cannot reconfigure workspace memory; restart ee to apply backend state."
        } else {
            " Setting applies when agent host starts."
        };
        self.push_workspace_memory_system(format!(
            "workspace memory {state}: persisted explicit `enabled = {enabled}` in {}. {data}{activation}",
            path.display()
        ));
    }

    pub(super) fn agents_memory_command(&mut self, args: &str) {
        let mut parts = args.splitn(2, char::is_whitespace);
        let operation = parts.next().unwrap_or_default();
        let argument = parts.next().unwrap_or_default().trim();
        match (operation, argument) {
            ("enable", "") => {
                self.persist_workspace_memory_switch(true);
                return;
            }
            ("disable", "") => {
                self.persist_workspace_memory_switch(false);
                return;
            }
            ("disable", "--delete") => {
                self.ensure_agents_host();
                let Some(host) = self.agents.host.as_ref() else {
                    self.backend.status_message =
                        Some(String::from("workspace memory host unavailable; nothing changed"));
                    return;
                };
                if !host.manager.workspace_memory_status().enabled {
                    self.backend.status_message = Some(String::from(
                        "workspace memory backend is not active; enable it and restart ee before disabling with deletion",
                    ));
                    return;
                }
                self.queue_workspace_memory_disable_delete(self.workspace_memory_config_path());
                return;
            }
            _ => {}
        }
        if operation == "status" && argument.is_empty() {
            self.ensure_agents_host();
            let Some(host) = self.agents.host.as_ref() else {
                self.backend.status_message =
                    Some(String::from("workspace memory host unavailable"));
                return;
            };
            let status = host.manager.workspace_memory_status();
            self.push_workspace_memory_system(format!(
                "workspace memory: enabled={} · availability={:?}\nactive: {} facts / {} bytes · trusted canonical roots: {} · workspace id: {}\nquotas: value={} bytes · active facts={} · active bytes={} · total facts={} · total bytes={} · recall={}\nretention: default expiry={} days · candidates={} days · stale/retracted={} days · superseded={} days\npersistence: local ee state directory, shared only by threads, sessions, agents, and ee processes using this canonical workspace identity; no repository storage or remote sync. Transcripts are never stored. Plain disable persists `enabled = false` and keeps database. `disable --delete` requires one-time confirmation, clears this workspace in backend first, then persists disable; clear failure leaves config enabled. Trust rules, autopilot, and bypass cannot skip confirmation. `forget` deletes every stored version; `retract` preserves retained history.",
                status.enabled,
                status.availability,
                status.active_facts,
                status.active_bytes,
                status.trusted_root_count,
                status.primary_workspace_id.as_deref().unwrap_or("unavailable"),
                status.quotas.max_value_bytes,
                status.quotas.max_active_facts,
                status.quotas.max_active_bytes,
                status.quotas.max_total_facts,
                status.quotas.max_total_bytes,
                status.quotas.max_recall_results,
                self.config.agents.workspace_memory.default_expiry_days,
                self.config.agents.workspace_memory.candidate_retention_days,
                self.config.agents.workspace_memory.stale_retention_days,
                self.config.agents.workspace_memory.superseded_retention_days,
            ));
            return;
        }
        if !self.config.agents.workspace_memory.enabled {
            self.backend.status_message = Some(String::from(
                "workspace memory disabled; explicitly set [agents.workspace_memory] enabled = true",
            ));
            return;
        }
        self.ensure_agents_host();
        let Some(host) = self.agents.host.as_ref() else {
            self.backend.status_message = Some(String::from("workspace memory host unavailable"));
            return;
        };
        let limit = self.config.agents.workspace_memory.max_recall_results;
        match (operation, argument) {
            ("list", "") => match host.manager.workspace_memory_list(limit) {
                Ok(result) => {
                    self.push_workspace_memory_system(Self::render_workspace_facts(&result))
                }
                Err(error) => {
                    self.backend.status_message =
                        Some(format!("workspace memory list failed: {error}"))
                }
            },
            ("search", query) if !query.is_empty() => {
                match host.manager.workspace_memory_recall(query, limit) {
                    Ok(result) => {
                        self.push_workspace_memory_system(Self::render_workspace_facts(&result))
                    }
                    Err(error) => {
                        self.backend.status_message =
                            Some(format!("workspace memory search failed: {error}"))
                    }
                }
            }
            ("show", key) if !key.is_empty() => match host.manager.workspace_memory_read(key) {
                Ok(fact) => self.push_workspace_memory_system(Self::render_workspace_fact(&fact)),
                Err(error) => {
                    self.backend.status_message =
                        Some(format!("workspace memory read failed: {error}"))
                }
            },
            ("forget", key) if !key.is_empty() => {
                self.queue_workspace_memory_forget(key.to_string())
            }
            ("retract", key) if !key.is_empty() => {
                self.queue_workspace_memory_retract(key.to_string())
            }
            ("clear", "") => self.queue_workspace_memory_clear(),
            ("export", "") => self.queue_workspace_memory_export(false),
            ("export", "--with-values") => self.queue_workspace_memory_export(true),
            ("import", path) if !path.is_empty() => match self.read_workspace_memory_import(path) {
                Ok((path, export)) => self.queue_workspace_memory_import(&path, export),
                Err(error) => {
                    self.backend.status_message =
                        Some(format!("workspace memory import failed: {error}"))
                }
            },
            _ => {
                self.backend.status_message = Some(String::from(
                    "usage: /memory enable|disable [--delete]|status|list|search <query>|show <key>|forget <key>|retract <key>|clear|export [--with-values]|import <path>",
                ))
            }
        }
    }

    pub(super) fn agents_context_command(&mut self, args: &str) {
        let mut parts = args.splitn(2, char::is_whitespace);
        match (parts.next().unwrap_or_default(), parts.next().unwrap_or_default().trim()) {
            ("", _) => self.agents_list_context_files(),
            ("status", "") => self.agents_context_status(),
            ("add", path) if !path.is_empty() => self.agents_add_context_file(path),
            ("remove", path) if !path.is_empty() => self.agents_remove_context_file(path),
            ("clear", "") => self.agents_clear_context_files(),
            _ => {
                self.backend.status_message =
                    Some(String::from("usage: /context [status|add <path>|remove <path>|clear]"));
            }
        }
    }
}

#[cfg(test)]
use super::format::{LOCAL_AGENT_SLASH_COMMANDS, LOCAL_AGENT_SLASH_HELP};

#[cfg(test)]
mod tests {
    use super::*;

    fn app_with_workspace_memory(temp: &tempfile::TempDir, enabled: bool) -> App {
        std::fs::write(
            temp.path().join(".ee.toml"),
            format!(
                "# preserve me\n[agents]\nenabled = true\n\n[agents.workspace_memory]\nenabled = {enabled}\ncandidate_retention_days = 11\n"
            ),
        )
        .unwrap();
        let _cwd_lock = crate::config::test_cwd_lock().lock().unwrap();
        let original = std::env::current_dir().unwrap();
        std::env::set_current_dir(temp.path()).unwrap();
        let app = App::from_path(None).unwrap();
        std::env::set_current_dir(original).unwrap();
        drop(_cwd_lock);
        app
    }

    #[test]
    fn workspace_memory_enable_and_plain_disable_persist_without_deletion() {
        let temp = tempfile::tempdir().unwrap();
        let mut app = app_with_workspace_memory(&temp, false);

        app.agents_memory_command("enable");
        assert!(app.config.agents.workspace_memory.enabled);
        let enabled = std::fs::read_to_string(temp.path().join(".ee.toml")).unwrap();
        assert!(enabled.contains("# preserve me"));
        assert!(enabled.contains("enabled = true\ncandidate_retention_days = 11"));

        app.agents_memory_command("disable");
        assert!(!app.config.agents.workspace_memory.enabled);
        let disabled = std::fs::read_to_string(temp.path().join(".ee.toml")).unwrap();
        assert!(disabled.contains("enabled = false\ncandidate_retention_days = 11"));
        assert!(app.agents.approvals.is_empty());
        assert!(
            app.backend.status_message.as_deref().is_some_and(|message| message.contains("kept"))
        );
    }

    #[test]
    fn workspace_memory_disable_delete_queues_one_time_confirmation_before_config_change() {
        let temp = tempfile::tempdir().unwrap();
        let mut app = app_with_workspace_memory(&temp, true);

        app.queue_workspace_memory_disable_delete(temp.path().join(".ee.toml"));

        assert_eq!(app.agents.approvals.len(), 1);
        let prompt = app.agents.approvals.front().unwrap();
        assert!(prompt.detail.contains("operation: disable --delete"));
        assert_eq!(
            prompt.options,
            vec![
                (String::from("Allow once"), super::super::agent_bridge::ApprovalChoice::AllowOnce),
                (String::from("Deny"), super::super::agent_bridge::ApprovalChoice::DenyOnce),
            ]
        );
        let contents = std::fs::read_to_string(temp.path().join(".ee.toml")).unwrap();
        assert!(contents.contains("enabled = true\ncandidate_retention_days = 11"));
    }

    #[test]
    fn workspace_memory_slash_registry_and_rendering_include_metadata() {
        assert!(LOCAL_AGENT_SLASH_COMMANDS.contains(&"memory"));
        assert!(
            LOCAL_AGENT_SLASH_HELP
                .iter()
                .any(|(command, _)| { command.starts_with("/memory enable|disable") })
        );
        let fact = ee_mcp::WorkspaceFact {
            id: 1,
            namespace: String::from("default"),
            key: String::from("build.command"),
            value: String::from("cargo test --quiet"),
            kind: String::from("command"),
            authority: String::from("user_asserted"),
            freshness: String::from("current"),
            state: String::from("active"),
            provenance: ee_mcp::WorkspaceFactProvenance {
                source_kind: String::from("user"),
                source_id: String::from("approval"),
                revision: None,
                fingerprint: None,
                verified_at: Some(String::from("2026-09-01T00:00:00Z")),
            },
            selection_reason: Some(String::from("exact_key")),
            created_at: String::from("2026-09-01T00:00:00Z"),
            updated_at: String::from("2026-09-01T00:00:00Z"),
            expires_at: None,
            content_hash: String::from("hash"),
            schema_version: 1,
        };
        let rendered = App::render_workspace_fact(&fact);
        for metadata in [
            "authority: user_asserted",
            "freshness: current",
            "state: active",
            "source: user:approval",
            "verified: 2026-09-01T00:00:00Z",
            "selection: exact_key",
        ] {
            assert!(rendered.contains(metadata), "missing {metadata}: {rendered}");
        }
    }
}
