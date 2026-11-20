//! Headless agent→client request handler.
//!
//! This is the headless counterpart of the editor bridge: file reads, file
//! writes, and terminal primitives without a TUI. Behavior is intentionally
//! different from the editor path in one documented way: writes go straight to
//! disk after approval instead of routing through buffer/edit/save state.
//!
//! Fail-closed rules:
//!
//! - every path is absolute and contained by a configured root; inside-root
//!   reads are the headless equivalent of the `mcp_safe_read` trust grant,
//!   and paths outside every root are never auto-granted;
//! - writes and terminal creation require an [`ApprovalGate`] answer;
//! - terminals are owned by the session that created them;
//! - elicitation is not advertised and is answered as unsupported.

mod fs_ops;
mod terminal;

use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;

use ee_agent_host::AgentError;
use ee_agent_host::{
    ClientRequest, ClientRequestHandler, ClientRequestResponse, ClientRequestResult,
    HandlerCapabilities,
};
use ee_agent_protocol::{CreateTerminalRequest, ReadTextFileRequest, WriteTextFileRequest};

use crate::permissions::{ApprovalGate, DenyAllGate, GateDecision, GateKind, GateRequest};

pub use fs_ops::{MAX_READ_BYTES, MAX_READ_RESPONSE_BYTES, MAX_WRITE_BYTES, PathPolicy};
pub use terminal::{
    DEFAULT_TERMINAL_OUTPUT_BYTES, DEFAULT_TERMINAL_WAIT_TIMEOUT, MAX_TERMINAL_ARGS,
    MAX_TERMINAL_COMMAND_CHARS, TerminalOptions, TerminalRegistry,
};

/// Headless handler configuration.
#[derive(Clone)]
pub struct HeadlessHandler {
    policy: PathPolicy,
    gate: Arc<dyn ApprovalGate>,
    terminals: TerminalRegistry,
}

impl std::fmt::Debug for HeadlessHandler {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("HeadlessHandler")
            .field("roots", &self.policy.roots())
            .field("terminals", &self.terminals)
            .finish_non_exhaustive()
    }
}

impl HeadlessHandler {
    /// Builds a handler over the allowed roots with a fail-closed gate.
    #[must_use]
    pub fn new(roots: Vec<PathBuf>, gate: Arc<dyn ApprovalGate>) -> Self {
        Self::with_terminal_options(roots, gate, TerminalOptions::default())
    }

    /// Builds a handler with explicit terminal bounds.
    #[must_use]
    pub fn with_terminal_options(
        roots: Vec<PathBuf>,
        gate: Arc<dyn ApprovalGate>,
        options: TerminalOptions,
    ) -> Self {
        Self { policy: PathPolicy::new(roots), gate, terminals: TerminalRegistry::new(options) }
    }

    /// Builds a handler that denies every gated operation.
    #[must_use]
    pub fn deny_all(roots: Vec<PathBuf>) -> Self {
        Self::new(roots, Arc::new(DenyAllGate))
    }

    /// The canonical root policy.
    #[must_use]
    pub fn policy(&self) -> &PathPolicy {
        &self.policy
    }

    /// The terminal registry (status and tests).
    #[must_use]
    pub fn terminals(&self) -> &TerminalRegistry {
        &self.terminals
    }

    async fn read_text(&self, request: &ReadTextFileRequest) -> Result<String, AgentError> {
        let path = self.policy.contain_existing(&request.path)?;
        let line = request.line;
        let limit = request.limit;
        tokio::task::spawn_blocking(move || fs_ops::read_text(&path, line, limit))
            .await
            .map_err(|error| AgentError::Io(format!("read task failed: {error}")))?
    }

    async fn write_text(&self, request: &WriteTextFileRequest) -> Result<(), AgentError> {
        if request.content.len() > MAX_WRITE_BYTES {
            return Err(AgentError::InvalidParams(format!(
                "write is {} bytes; the headless cap is {MAX_WRITE_BYTES}",
                request.content.len()
            )));
        }
        let path = self.policy.contain_new(&request.path)?;
        let summary =
            bounded_summary(&format!("write {} ({} bytes)", path.display(), request.content.len()));
        self.require_approval(
            &request.session_id,
            summary,
            GateKind::WriteFile { path: path.clone(), bytes: request.content.len() },
        )
        .await?;
        tokio::fs::write(&path, request.content.as_bytes())
            .await
            .map_err(|error| AgentError::Io(format!("{}: {error}", path.display())))
    }

    async fn create_terminal(&self, request: &CreateTerminalRequest) -> Result<String, AgentError> {
        let cwd = match &request.cwd {
            Some(cwd) => self.policy.contain_directory(cwd)?,
            None => self.default_cwd()?,
        };
        let summary = bounded_summary(&format!("run {}", request.command));
        self.require_approval(
            &request.session_id,
            summary,
            GateKind::CreateTerminal {
                command: request.command.clone(),
                args: request.args.clone(),
                cwd: cwd.clone(),
            },
        )
        .await?;
        let response = self.terminals.create(request, &cwd)?;
        Ok(response.terminal_id.0.to_string())
    }

    fn default_cwd(&self) -> Result<PathBuf, AgentError> {
        self.policy.roots().first().cloned().ok_or_else(|| AgentError::PermissionDenied {
            reason: String::from("no workspace root is configured for headless terminals"),
        })
    }

    async fn require_approval(
        &self,
        session_id: &ee_agent_protocol::SessionId,
        summary: String,
        kind: GateKind,
    ) -> Result<(), AgentError> {
        let request = GateRequest { session_id: session_id.clone(), summary, kind };
        match self.gate.authorize(request).await {
            GateDecision::Allow => Ok(()),
            GateDecision::Deny => Err(AgentError::PermissionDenied {
                reason: String::from("headless operation was not approved"),
            }),
        }
    }
}

fn bounded_summary(text: &str) -> String {
    const MAX_SUMMARY_CHARS: usize = 512;
    let mut chars = text.chars();
    let summary: String = chars.by_ref().take(MAX_SUMMARY_CHARS).collect();
    if chars.next().is_some() { format!("{summary}…") } else { summary }
}

fn unsupported(method: &str) -> AgentError {
    AgentError::CapabilityUnsupported { method: method.to_string() }
}

impl ClientRequestHandler for HeadlessHandler {
    fn capabilities(&self) -> HandlerCapabilities {
        HandlerCapabilities {
            fs_read: true,
            fs_write: true,
            terminal: true,
            ..HandlerCapabilities::none()
        }
    }

    fn handle(
        &self,
        request: ClientRequest,
    ) -> Pin<Box<dyn Future<Output = ClientRequestResult> + Send + '_>> {
        Box::pin(async move {
            match request {
                ClientRequest::ReadTextFile(request) => Ok(ClientRequestResponse::ReadTextFile(
                    ee_agent_protocol::ReadTextFileResponse::new(self.read_text(&request).await?),
                )),
                ClientRequest::WriteTextFile(request) => {
                    self.write_text(&request).await?;
                    Ok(ClientRequestResponse::WriteTextFile(
                        ee_agent_protocol::WriteTextFileResponse::new(),
                    ))
                }
                ClientRequest::CreateTerminal(request) => {
                    let terminal_id = self.create_terminal(&request).await?;
                    Ok(ClientRequestResponse::CreateTerminal(
                        ee_agent_protocol::CreateTerminalResponse::new(
                            ee_agent_protocol::TerminalId::new(terminal_id),
                        ),
                    ))
                }
                ClientRequest::TerminalOutput(request) => {
                    let (output, truncated, exit_status) = self
                        .terminals
                        .output(&request.terminal_id, request.session_id.0.as_ref())?;
                    let mut response =
                        ee_agent_protocol::TerminalOutputResponse::new(output, truncated);
                    if let Some(exit_status) = exit_status {
                        response = response.exit_status(exit_status);
                    }
                    Ok(ClientRequestResponse::TerminalOutput(response))
                }
                ClientRequest::WaitForTerminalExit(request) => {
                    let status = self
                        .terminals
                        .wait(&request.terminal_id, request.session_id.0.as_ref())
                        .await?;
                    Ok(ClientRequestResponse::WaitForTerminalExit(
                        ee_agent_protocol::WaitForTerminalExitResponse::new(status),
                    ))
                }
                ClientRequest::KillTerminal(request) => {
                    self.terminals.kill(&request.terminal_id, request.session_id.0.as_ref())?;
                    Ok(ClientRequestResponse::KillTerminal(
                        ee_agent_protocol::KillTerminalResponse::new(),
                    ))
                }
                ClientRequest::ReleaseTerminal(request) => {
                    self.terminals.release(&request.terminal_id, request.session_id.0.as_ref())?;
                    Ok(ClientRequestResponse::ReleaseTerminal(
                        ee_agent_protocol::ReleaseTerminalResponse::new(),
                    ))
                }
                ClientRequest::CreateElicitation(_) => {
                    Err(unsupported(ee_agent_protocol::ELICITATION_CREATE_METHOD_NAME))
                }
                other => Err(unsupported(other.method())),
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::time::Duration;

    use ee_agent_protocol::SessionId;
    use tempfile::tempdir;

    use super::*;
    use crate::permissions::gate::GateDecision;

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    enum Answer {
        Allow,
        Deny,
        DenyAfter(Duration),
    }

    struct ScriptedGate(Answer);

    impl ApprovalGate for ScriptedGate {
        fn authorize(
            &self,
            _request: GateRequest,
        ) -> Pin<Box<dyn Future<Output = GateDecision> + Send + '_>> {
            Box::pin(async move {
                match self.0 {
                    Answer::Allow => GateDecision::Allow,
                    Answer::Deny => GateDecision::Deny,
                    Answer::DenyAfter(delay) => {
                        tokio::time::sleep(delay).await;
                        GateDecision::Deny
                    }
                }
            })
        }
    }

    fn handler(root: &Path, answer: Answer) -> HeadlessHandler {
        HeadlessHandler::new(vec![root.to_path_buf()], Arc::new(ScriptedGate(answer)))
    }

    async fn call(handler: &HeadlessHandler, request: ClientRequest) -> ClientRequestResult {
        handler.handle(request).await
    }

    #[tokio::test]
    async fn reads_inside_the_root_and_denies_outside() {
        let inside = tempdir().expect("inside");
        let outside = tempdir().expect("outside");
        let inside_file = inside.path().join("inside.txt");
        std::fs::write(&inside_file, "inside").expect("write");
        let outside_file = outside.path().join("outside.txt");
        std::fs::write(&outside_file, "outside").expect("write");
        let handler = handler(inside.path(), Answer::Deny);

        let response = call(
            &handler,
            ClientRequest::ReadTextFile(ReadTextFileRequest::new(
                SessionId::new("s-1"),
                &inside_file,
            )),
        )
        .await
        .expect("inside read");
        assert!(matches!(
            response,
            ClientRequestResponse::ReadTextFile(response) if response.content == "inside"
        ));

        let error = call(
            &handler,
            ClientRequest::ReadTextFile(ReadTextFileRequest::new(
                SessionId::new("s-1"),
                &outside_file,
            )),
        )
        .await
        .expect_err("outside read");
        assert!(matches!(error, AgentError::PermissionDenied { .. }));
    }

    #[tokio::test]
    async fn writes_require_approval() {
        let dir = tempdir().expect("dir");
        let target = dir.path().join("new.txt");

        let denied = handler(dir.path(), Answer::Deny);
        let error = call(
            &denied,
            ClientRequest::WriteTextFile(WriteTextFileRequest::new(
                SessionId::new("s-1"),
                &target,
                "data",
            )),
        )
        .await
        .expect_err("denied write");
        assert!(matches!(error, AgentError::PermissionDenied { .. }));
        assert!(!target.exists());

        let allowed = handler(dir.path(), Answer::Allow);
        call(
            &allowed,
            ClientRequest::WriteTextFile(WriteTextFileRequest::new(
                SessionId::new("s-1"),
                &target,
                "data",
            )),
        )
        .await
        .expect("allowed write");
        assert_eq!(std::fs::read_to_string(&target).expect("read"), "data");
    }

    #[tokio::test]
    async fn an_approval_that_never_answers_ends_denied() {
        let dir = tempdir().expect("dir");
        let target = dir.path().join("new.txt");
        let handler = handler(dir.path(), Answer::DenyAfter(Duration::from_millis(25)));
        let error = call(
            &handler,
            ClientRequest::WriteTextFile(WriteTextFileRequest::new(
                SessionId::new("s-1"),
                &target,
                "data",
            )),
        )
        .await
        .expect_err("timed-out approval");
        assert!(matches!(error, AgentError::PermissionDenied { .. }));
        assert!(!target.exists());
    }

    #[tokio::test]
    async fn terminal_creation_is_gated_and_owned() {
        let dir = tempdir().expect("dir");
        let request = CreateTerminalRequest::new(SessionId::new("s-1"), "/bin/sh")
            .args(vec!["-c".to_string(), "printf hello".to_string()]);

        let denied = handler(dir.path(), Answer::Deny);
        let error = call(&denied, ClientRequest::CreateTerminal(request.clone()))
            .await
            .expect_err("denied terminal");
        assert!(matches!(error, AgentError::PermissionDenied { .. }));
        assert!(denied.terminals().is_empty());

        let allowed = handler(dir.path(), Answer::Allow);
        let response =
            call(&allowed, ClientRequest::CreateTerminal(request)).await.expect("allowed terminal");
        let ClientRequestResponse::CreateTerminal(response) = response else {
            panic!("unexpected response");
        };
        let terminal_id = response.terminal_id;

        let status = allowed.terminals().wait(&terminal_id, "s-1").await.expect("wait");
        assert_eq!(status.exit_code, Some(0));

        let mut output = String::new();
        for _ in 0..50 {
            let (text, _, _) = allowed.terminals().output(&terminal_id, "s-1").expect("output");
            output = text;
            if output.contains("hello") {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(output.contains("hello"), "output was {output:?}");

        let error = allowed.terminals().output(&terminal_id, "s-2").expect_err("ownership");
        assert!(matches!(error, AgentError::PermissionDenied { .. }));
        assert!(!allowed.terminals().kill(&terminal_id, "s-1").expect("second kill"));
        assert!(!allowed.terminals().kill(&terminal_id, "s-1").expect("third kill"));

        allowed.terminals().release(&terminal_id, "s-1").expect("release");
        assert!(allowed.terminals().is_empty());
    }

    #[tokio::test]
    async fn unsupported_capabilities_fail_closed() {
        let dir = tempdir().expect("dir");
        let handler = handler(dir.path(), Answer::Allow);
        let error = handler
            .handle(ClientRequest::ApproveWorkspaceMemoryMutation {
                operation: ee_agent_host::WorkspaceMemoryMutationOperation::Remember,
                key: String::from("k"),
            })
            .await
            .expect_err("unsupported");
        assert!(matches!(error, AgentError::CapabilityUnsupported { .. }));
    }

    #[test]
    fn capabilities_advertise_files_and_terminals_only() {
        let capabilities = HeadlessHandler::deny_all(Vec::new()).capabilities();
        assert!(capabilities.fs_read);
        assert!(capabilities.fs_write);
        assert!(capabilities.terminal);
        assert!(!capabilities.elicitation_form);
        assert!(!capabilities.elicitation_url);
        assert!(!capabilities.proxy_discovery);
    }
}
