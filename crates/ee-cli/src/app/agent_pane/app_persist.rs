//! `impl App`: persisted workspace save/restore, session record paths.

use std::collections::{BTreeSet, HashMap};
use std::path::PathBuf;

use super::super::*;

use super::constants::AGENT_LIFECYCLE_CONCURRENCY;
use super::state::{
    PendingSession, PersistedAgentSession, PersistedAgentWorkspace,
    PersistedAgentWorkspaceDocument, WorkspaceRestore, persisted_agent_workspace_version,
};
use crate::agent_session_store::{SessionArchiveRecord, WorkspaceSessionArchive};
impl App {
    /// Starts restoring every persisted workspace thread. Returns `true` when
    /// at least one session was queued instead of creating a fresh session.
    pub(super) fn start_workspace_restore(&mut self) -> bool {
        let Some(workspace) = self.load_persisted_agent_workspace() else {
            return false;
        };
        if workspace.sessions.is_empty() {
            self.agents.archived_persisted = workspace.archived_sessions.clone();
            self.migrate_archived_transcripts();
            return false;
        }
        self.agents.archived_persisted = workspace.archived_sessions.clone();
        self.migrate_archived_transcripts();
        let order = workspace
            .sessions
            .iter()
            .enumerate()
            .map(|(index, record)| (record.session_id.clone(), index))
            .collect();
        self.agents.workspace_restore = Some(WorkspaceRestore {
            active_session_id: workspace.active_session_id,
            sessions: workspace.sessions.into(),
            order,
            in_flight: BTreeSet::new(),
            failed: false,
        });
        self.start_next_workspace_restore();
        true
    }

    /// Fills bounded restoration slots. Per-connection drivers retain ACP
    /// ordering; independent sessions and connections may overlap.
    pub(super) fn start_next_workspace_restore(&mut self) {
        loop {
            let next = {
                let Some(restore) = self.agents.workspace_restore.as_mut() else {
                    return;
                };
                if restore.in_flight.len() >= AGENT_LIFECYCLE_CONCURRENCY {
                    return;
                }
                restore.sessions.pop_front()
            };
            let Some(record) = next else {
                break;
            };
            let duplicate = self.agents.thread_index(&record.session_id).is_some()
                || self.agents.pending_sessions.keys().any(|key| {
                    key.agent_id == record.agent_id
                        && key.session_id.as_deref() == Some(record.session_id.as_str())
                });
            if duplicate {
                if let Some(restore) = self.agents.workspace_restore.as_mut() {
                    restore.failed = true;
                }
                self.agents.error =
                    Some(format!("duplicate persisted session ignored: {}", record.session_id));
                continue;
            }
            if let Some(restore) = self.agents.workspace_restore.as_mut() {
                restore.in_flight.insert(record.session_id.clone());
            }
            self.request_persisted_agent_reconnect(record);
        }

        let finished = self
            .agents
            .workspace_restore
            .as_ref()
            .is_some_and(|restore| restore.sessions.is_empty() && restore.in_flight.is_empty());
        if !finished {
            return;
        }
        let restore = self.agents.workspace_restore.take().expect("finished restore present");
        self.agents.threads.sort_by_key(|thread| {
            restore.order.get(&thread.session_id).copied().unwrap_or(usize::MAX)
        });
        self.agents.active_thread = restore
            .active_session_id
            .as_deref()
            .and_then(|session_id| self.agents.thread_index(session_id))
            .or_else(|| (!self.agents.threads.is_empty()).then_some(0));
        if !restore.failed {
            self.persist_agent_workspace();
        }
    }

    /// Enqueues one reconnect through the existing load-then-resume pipeline.
    pub(super) fn request_persisted_agent_reconnect(&mut self, record: PersistedAgentSession) {
        if self.agents.pending_sessions.keys().any(|key| {
            key.agent_id == record.agent_id
                && key.session_id.as_deref() == Some(record.session_id.as_str())
        }) {
            self.agents.error =
                Some(format!("session {} reconnect is already in progress", record.session_id));
            return;
        }
        self.ensure_agents_host();
        if self.agents.host.is_none() {
            return;
        }
        // `session/load` replays the conversation while the reply is still
        // in flight; those updates are buffered until the thread exists.
        self.agents.pending_replay.insert(record.session_id.clone(), Vec::new());
        let roots = self.agents_workspace_roots();
        let cwd = roots.first().cloned().unwrap_or_else(|| self.working_dir.clone());
        let additional_directories = roots.iter().skip(1).cloned().collect();
        // Resolve MCP entries (and redaction values) before borrowing the host.
        let mcp_servers = self.mcp_forward_entries();
        let ee_proxy_stdio_fallback =
            self.agents.mcp.proxy.as_ref().map(crate::app::agents_mcp::proxy_stdio_fallback_entry);
        let Some(host) = &self.agents.host else {
            return;
        };
        let reply = host.request_reconnect(
            record.agent_id.clone(),
            record.session_id.clone(),
            cwd,
            additional_directories,
            mcp_servers,
            ee_proxy_stdio_fallback,
        );
        let key = self
            .agents
            .next_lifecycle_key(record.agent_id.clone(), Some(record.session_id.clone()));
        self.agents.pending_sessions.insert(key, PendingSession { reply, fork: None });
        self.backend.status_message =
            Some(format!("reconnecting session {}...", record.session_id));
    }

    /// Opens the per-workspace transcript journal lazily. The store owns
    /// rotating owner-only files under the user data directory; tests use the
    /// injected state base so they never touch real user data.
    pub(super) fn workspace_session_archive(&self) -> Option<&WorkspaceSessionArchive> {
        self.agents
            .session_archive
            .get_or_init(|| {
                let root = self.session_archive_root()?;
                match WorkspaceSessionArchive::open(root, self.primary_workspace_identity()) {
                    Ok(archive) => Some(archive),
                    Err(error) => {
                        eprintln!("ee: agent transcript archive unavailable: {error}");
                        None
                    }
                }
            })
            .as_ref()
    }

    fn session_archive_root(&self) -> Option<PathBuf> {
        #[cfg(test)]
        {
            self.agents.test_session_state_base.as_ref().map(|base| base.join("agent-sessions"))
        }
        #[cfg(not(test))]
        {
            WorkspaceSessionArchive::default_root()
        }
    }

    /// Imports legacy JSON transcript fields into the journal once, so the
    /// registry can drop them on the next successful save without data loss.
    fn migrate_archived_transcripts(&self) {
        let Some(archive) = self.workspace_session_archive() else {
            return;
        };
        for record in &self.agents.archived_persisted {
            if record.transcript.is_empty() {
                continue;
            }
            if archive.load_transcript(&record.session_id).ok().flatten().is_none() {
                let archive_record = SessionArchiveRecord {
                    agent_id: record.agent_id.clone(),
                    session_id: record.session_id.clone(),
                    session_name: record.session_name.clone(),
                    archived: true,
                };
                if let Err(error) = archive.save(&archive_record, &record.transcript) {
                    eprintln!(
                        "ee: agent transcript migration failed for {}: {error}",
                        record.session_id
                    );
                }
            }
        }
    }
    /// The path of the client-persisted agent session record (per-workspace
    /// entries under the platform state directory).
    fn agents_session_record_path(&self) -> Option<std::path::PathBuf> {
        #[cfg(test)]
        if let Some(base) = self.agents.test_session_state_base.as_deref() {
            return Some(base.join("agent-sessions.json"));
        }
        crate::logs::state_dir().map(|dir| dir.join("agent-sessions.json"))
    }

    /// Loads persisted ordered threads for the primary workspace. Legacy
    /// one-session documents remain readable and migrate on their next write.
    pub(super) fn load_persisted_agent_workspace(&self) -> Option<PersistedAgentWorkspace> {
        let path = self.agents_session_record_path()?;
        let documents: HashMap<String, serde_json::Value> =
            serde_json::from_str(&std::fs::read_to_string(path).ok()?).ok()?;
        let document = documents.get(&self.primary_workspace_identity().as_string())?.clone();
        serde_json::from_value::<PersistedAgentWorkspaceDocument>(document)
            .ok()
            .map(PersistedAgentWorkspaceDocument::into_workspace)
    }

    /// Writes one complete workspace thread registry atomically at the
    /// document level, preserving records for every other workspace.
    fn save_persisted_agent_workspace(&self, workspace: Option<&PersistedAgentWorkspace>) {
        let Some(path) = self.agents_session_record_path() else {
            return;
        };
        let mut documents: HashMap<String, serde_json::Value> = std::fs::read_to_string(&path)
            .ok()
            .and_then(|text| serde_json::from_str(&text).ok())
            .unwrap_or_default();
        let key = self.primary_workspace_identity().as_string();
        match workspace {
            Some(workspace) => match serde_json::to_value(workspace) {
                Ok(value) => {
                    documents.insert(key, value);
                }
                Err(_) => return,
            },
            None => {
                documents.remove(&key);
            }
        }
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if let Ok(json) = serde_json::to_string(&documents) {
            let _ = std::fs::write(path, json);
        }
    }

    /// Persists current open-thread ordering, local names, selected thread,
    /// and per-thread recoverable prompts. Archived threads are stored as
    /// metadata here; their transcript bodies live in the rotating journal.
    pub(super) fn persist_agent_workspace(&self) {
        // Keep queued restore records intact until every ACP load completes.
        // Otherwise registering the first restored thread would erase peers
        // that have not yet been attempted.
        if self.agents.workspace_restore.is_some() {
            return;
        }
        let existing = self.load_persisted_agent_workspace().unwrap_or_default();
        let archive = self.workspace_session_archive();
        let sessions = self
            .agents
            .threads
            .iter()
            .map(|thread| PersistedAgentSession {
                agent_id: thread.agent_id.clone(),
                session_id: thread.session_id.clone(),
                last_prompt: existing
                    .sessions
                    .iter()
                    .find(|record| record.session_id == thread.session_id)
                    .and_then(|record| record.last_prompt.clone()),
                session_name: thread.session_name.clone(),
                transcript: match archive {
                    Some(archive) => {
                        let record = SessionArchiveRecord::new(
                            thread.agent_id.clone(),
                            thread.session_id.clone(),
                            thread.session_name.clone(),
                        );
                        match archive.save(&record, &thread.transcript) {
                            Ok(()) => Vec::new(),
                            Err(error) => {
                                eprintln!(
                                    "ee: agent transcript save failed for {}: {error}",
                                    thread.session_id
                                );
                                // Keep a full registry fallback rather than
                                // silently losing a transcript the journal
                                // could not accept.
                                thread.transcript.clone()
                            }
                        }
                    }
                    // No durable store (tests without an injected base, or an
                    // unusable data directory): keep the current fallback.
                    None => thread.transcript.clone(),
                },
            })
            .collect();
        let active_session_id = self
            .agents
            .workspace_restore
            .as_ref()
            .and_then(|restore| restore.active_session_id.clone())
            .or_else(|| {
                self.agents
                    .active_thread_index()
                    .and_then(|index| self.agents.threads.get(index))
                    .map(|thread| thread.session_id.clone())
            });
        self.save_persisted_agent_workspace(Some(&PersistedAgentWorkspace {
            version: persisted_agent_workspace_version(),
            active_session_id,
            sessions,
            archived_sessions: self.agents.archived_persisted.clone(),
        }));
    }

    /// Updates a persisted thread's recoverable prompt without disturbing
    /// other workspace sessions.
    pub(super) fn update_persisted_last_prompt(&self, session_id: &str, last_prompt: Option<&str>) {
        let Some(mut workspace) = self.load_persisted_agent_workspace() else {
            return;
        };
        let Some(record) =
            workspace.sessions.iter_mut().find(|record| record.session_id == session_id)
        else {
            return;
        };
        record.last_prompt = last_prompt.map(str::to_string);
        self.save_persisted_agent_workspace(Some(&workspace));
    }

    /// Absolute workspace roots forwarded as ACP session context.
    pub(crate) fn agents_workspace_roots(&self) -> Vec<PathBuf> {
        let mut roots = vec![self.working_dir.clone()];
        for buf in self.backend.all_bufs() {
            if let Some(path) = &buf.path
                && let Some(parent) = path.parent()
            {
                roots.push(parent.to_path_buf());
            }
        }
        roots.extend(self.agents.additional_workspace_roots.iter().cloned());
        roots.sort();
        roots.dedup();
        roots
    }
}
