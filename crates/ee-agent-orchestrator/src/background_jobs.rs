//! Background terminal job registry.
//!
//! Terminals created through the client bridge keep running after
//! `create_terminal` returns. This registry records those jobs, projects their
//! status into the runtime snapshot, and polls the bridge at each model-step
//! boundary so an exited job surfaces a bounded system notice without the
//! model blocking on `wait_for_terminal_exit`.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use ee_agent_acp_server::ClientBridge;
use ee_agent_protocol::{
    KillTerminalRequest, ReleaseTerminalRequest, SessionId, TerminalId, TerminalOutputRequest,
};

/// Maximum bytes of terminal output retained in one completion notice.
pub const BACKGROUND_JOB_TAIL_BYTES: usize = 2_000;

/// Lifecycle state of one background terminal job.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BackgroundJobStatus {
    /// Command is still running.
    Running,
    /// Command exited; the code is unknown when the host reported only a signal.
    Exited {
        /// Process exit code, when the host reported one.
        exit_code: Option<u32>,
    },
    /// Job was killed by session cancellation or `/discard`.
    Terminated,
}

/// One background terminal job owned by the runtime.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BackgroundJob {
    /// Host terminal id.
    pub terminal_id: String,
    /// Command line the model requested.
    pub command: String,
    /// Current lifecycle state.
    pub status: BackgroundJobStatus,
    /// Bounded tail of captured output, filled on exit.
    pub tail: String,
    /// Whether the model was already told the job exited.
    pub noticed: bool,
}

impl BackgroundJob {
    fn new(terminal_id: impl Into<String>, command: impl Into<String>) -> Self {
        Self {
            terminal_id: terminal_id.into(),
            command: command.into(),
            status: BackgroundJobStatus::Running,
            tail: String::new(),
            noticed: false,
        }
    }

    fn status_line(&self) -> String {
        match self.status {
            BackgroundJobStatus::Running => String::from("running"),
            BackgroundJobStatus::Exited { exit_code: Some(code) } => format!("exited ({code})"),
            BackgroundJobStatus::Exited { exit_code: None } => String::from("exited (unknown)"),
            BackgroundJobStatus::Terminated => String::from("terminated"),
        }
    }
}

/// Shared per-runtime background terminal job registry.
#[derive(Debug, Default)]
pub struct BackgroundJobRegistry {
    jobs: Mutex<BTreeMap<String, BackgroundJob>>,
}

impl BackgroundJobRegistry {
    /// Creates an empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Records a terminal as a background job. Re-registering the same
    /// terminal id keeps the first command and state.
    pub fn register(&self, terminal_id: impl Into<String>, command: impl Into<String>) {
        let mut jobs = self.jobs.lock().expect("background jobs poisoned");
        jobs.entry(terminal_id.into())
            .or_insert_with_key(|id| BackgroundJob::new(id.clone(), command));
    }

    /// Removes a job because the model waited on it in the foreground; the
    /// wait result already carries the exit status and output tail.
    pub fn note_foreground_wait(&self, terminal_id: &str) {
        self.jobs.lock().expect("background jobs poisoned").remove(terminal_id);
    }

    /// Whether any job is currently tracked.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.jobs.lock().expect("background jobs poisoned").is_empty()
    }

    /// Drops all tracked jobs without touching the host terminals.
    pub fn clear(&self) {
        self.jobs.lock().expect("background jobs poisoned").clear();
    }

    /// Renders the deterministic `BACKGROUND_JOBS` runtime-context section.
    /// Returns `None` when no job is tracked so an idle session does not pay
    /// for an empty section.
    #[must_use]
    pub fn snapshot_text(&self) -> Option<String> {
        let jobs = self.jobs.lock().expect("background jobs poisoned");
        if jobs.is_empty() {
            return None;
        }
        let mut text = String::from(
            "Background terminal jobs. Exit notices also arrive as system reminders between steps.",
        );
        for job in jobs.values() {
            text.push_str(&format!(
                "\n- terminal {} [{}]: {}",
                job.terminal_id,
                job.status_line(),
                job.command
            ));
        }
        Some(text)
    }

    /// Polls every running job once and returns model-visible notices for jobs
    /// that exited since the previous poll.
    ///
    /// A bridge failure is treated as "still running": the host may not expose
    /// output for a terminal that is still live, and a transient transport
    /// error must not fabricate an exit.
    pub async fn poll(&self, client: &ClientBridge, session_id: &SessionId) -> Vec<String> {
        let running: Vec<(String, String)> = {
            let jobs = self.jobs.lock().expect("background jobs poisoned");
            jobs.values()
                .filter(|job| job.status == BackgroundJobStatus::Running)
                .map(|job| (job.terminal_id.clone(), job.command.clone()))
                .collect()
        };
        if running.is_empty() {
            return Vec::new();
        }
        let mut notices = Vec::new();
        for (terminal_id, command) in running {
            let response = client
                .terminal_output(TerminalOutputRequest::new(
                    session_id.clone(),
                    TerminalId::new(terminal_id.clone()),
                ))
                .await;
            let Ok(response) = response else {
                continue;
            };
            let Some(exit_code) = response.exit_status.as_ref().map(|status| status.exit_code)
            else {
                continue;
            };
            let tail = tail_bytes(&response.output);
            let mut jobs = self.jobs.lock().expect("background jobs poisoned");
            let Some(job) = jobs.get_mut(&terminal_id) else {
                continue;
            };
            job.status = BackgroundJobStatus::Exited { exit_code };
            job.tail = tail;
            if job.noticed {
                continue;
            }
            job.noticed = true;
            let code = exit_code.map_or_else(|| String::from("unknown"), |code| code.to_string());
            let mut notice = format!(
                "[background job exited] terminal {terminal_id} ({command}) exited with code {code}."
            );
            if !job.tail.trim().is_empty() {
                notice.push_str("\nOutput tail:\n");
                notice.push_str(job.tail.trim_end());
            }
            notices.push(notice);
        }
        notices
    }

    /// Kills and releases every running job, then clears the registry.
    ///
    /// Used on `session/cancel` and `/discard` so no background child outlives
    /// its session.
    pub async fn terminate_all(&self, client: &ClientBridge, session_id: &SessionId) {
        let terminal_ids: Vec<String> = {
            let jobs = self.jobs.lock().expect("background jobs poisoned");
            jobs.values()
                .filter(|job| job.status == BackgroundJobStatus::Running)
                .map(|job| job.terminal_id.clone())
                .collect()
        };
        for terminal_id in terminal_ids {
            let terminal = TerminalId::new(terminal_id);
            let _ = client
                .kill_terminal(KillTerminalRequest::new(session_id.clone(), terminal.clone()))
                .await;
            let _ = client
                .release_terminal(ReleaseTerminalRequest::new(session_id.clone(), terminal))
                .await;
        }
        self.clear();
    }
}

/// Shared handle used by the runtime, loop engine, and tool executor.
pub type SharedBackgroundJobs = Arc<BackgroundJobRegistry>;

fn tail_bytes(text: &str) -> String {
    if text.len() <= BACKGROUND_JOB_TAIL_BYTES {
        return text.to_string();
    }
    let mut start = text.len() - BACKGROUND_JOB_TAIL_BYTES;
    while !text.is_char_boundary(start) {
        start += 1;
    }
    text[start..].to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_registry_emits_no_section() {
        let registry = BackgroundJobRegistry::new();
        assert!(registry.snapshot_text().is_none());
        assert!(registry.is_empty());
    }

    #[test]
    fn snapshot_is_sorted_and_reports_exit_status() {
        let registry = BackgroundJobRegistry::new();
        registry.register("term-b", "cargo test");
        registry.register("term-a", "cargo build");
        registry.note_foreground_wait("term-b");
        assert_eq!(
            registry.snapshot_text().as_deref(),
            Some(
                "Background terminal jobs. Exit notices also arrive as system reminders between steps.\n- terminal term-a [running]: cargo build"
            )
        );
    }

    #[test]
    fn tail_truncates_at_char_boundaries() {
        let text = "é".repeat(BACKGROUND_JOB_TAIL_BYTES);
        let tail = tail_bytes(&text);
        assert!(tail.len() <= BACKGROUND_JOB_TAIL_BYTES);
        assert!(tail.chars().all(|ch| ch == 'é'));
    }

    #[test]
    fn foreground_wait_removes_the_job() {
        let registry = BackgroundJobRegistry::new();
        registry.register("term-1", "cargo build");
        registry.note_foreground_wait("term-1");
        assert!(registry.is_empty());
    }
}
