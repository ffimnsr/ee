//! `impl App`: test-facing queues/hooks for approval and write verification.

#[cfg(test)]
use std::sync::atomic::Ordering;

use super::super::*;

#[cfg(test)]
use super::WEB_DISPATCH_TEST_COUNT;

use super::approval::ApprovalChoice;

impl App {
    #[cfg(test)]
    pub(crate) fn queue_terminal_approval_for_test(
        &mut self,
        session_id: &str,
        agent_id: Option<&str>,
        command: &str,
        args: &[&str],
        env: &[(&str, &str)],
        cwd: Option<PathBuf>,
    ) -> oneshot::Receiver<ClientRequestResult> {
        let request = CreateTerminalRequest::new(SessionId::new(session_id), command)
            .args(args.iter().map(|value| (*value).to_string()).collect())
            .env(env.iter().map(|(name, value)| EnvVariable::new(*name, *value)).collect())
            .cwd(cwd);
        let persistent_allowed = self.command_invocation_for_request(&request).is_ok();
        let (reply, receiver) = oneshot::channel();
        self.request_bridge_approval(ApprovalPrompt::terminal(
            None,
            agent_id.map(str::to_string),
            &SessionId::new(session_id),
            &request,
            reply,
            persistent_allowed,
        ));
        receiver
    }

    #[cfg(test)]
    pub(crate) fn queue_write_approval_for_test(
        &mut self,
        path: PathBuf,
        content: &str,
    ) -> oneshot::Receiver<ClientRequestResult> {
        let request =
            WriteTextFileRequest::new(SessionId::new("persistent-deny-write"), path, content);
        let (reply, receiver) = oneshot::channel();
        self.request_bridge_approval(ApprovalPrompt::write(
            None,
            &request.session_id,
            &request,
            None,
            reply,
        ));
        receiver
    }

    #[cfg(test)]
    pub(crate) fn queue_session_write_approval_for_test(
        &mut self,
        agent_id: &str,
        session_id: &str,
        path: PathBuf,
        content: &str,
    ) -> oneshot::Receiver<ClientRequestResult> {
        let request = WriteTextFileRequest::new(SessionId::new(session_id), path, content);
        let (reply, receiver) = oneshot::channel();
        let mut prompt = ApprovalPrompt::write(None, &request.session_id, &request, None, reply);
        prompt.agent_id = Some(agent_id.to_string());
        self.request_bridge_approval(prompt);
        receiver
    }

    #[cfg(test)]
    pub(crate) fn queue_filesystem_create_approval_for_test(
        &mut self,
        path: PathBuf,
    ) -> oneshot::Receiver<ClientRequestResult> {
        let (reply, receiver) = oneshot::channel();
        self.queue_proxy_filesystem(
            crate::app::agent_filesystem::FilesystemOperation::CreateDirectory { path },
            reply,
        );
        receiver
    }

    #[cfg(test)]
    pub(crate) fn queue_filesystem_delete_approval_for_test(
        &mut self,
        path: PathBuf,
    ) -> oneshot::Receiver<ClientRequestResult> {
        let (reply, receiver) = oneshot::channel();
        self.queue_proxy_filesystem(
            crate::app::agent_filesystem::FilesystemOperation::DeletePath { path },
            reply,
        );
        receiver
    }

    #[cfg(test)]
    pub(crate) fn queue_network_fetch_approval_for_test(
        &mut self,
        host: &str,
    ) -> oneshot::Receiver<ClientRequestResult> {
        let (reply, receiver) = oneshot::channel();
        self.request_web_approval(ApprovalPrompt::web(
            ProxyRoute::Stdio,
            String::from("proxy-network:stdio:ee --mcp-proxy:persistent-deny-test"),
            host.to_string(),
            host.to_string(),
            None,
            WebApprovalCall::Fetch { url: format!("https://{host}/blocked") },
            BTreeSet::new(),
            CancellationToken::new(),
            reply,
        ));
        receiver
    }

    #[cfg(test)]
    pub(crate) fn queue_generic_mcp_write_approval_for_test(
        &mut self,
        path: PathBuf,
        content: &str,
    ) -> oneshot::Receiver<ClientRequestResult> {
        let invocation = self
            .mcp_invocation_for_tool(
                "ee_format_file",
                serde_json::json!({ "path": path }),
                ProxyRoute::Stdio,
            )
            .expect("format tool must have exact MCP identity");
        let spec = ProxyWriteSpec {
            title: String::from("ee_format_file"),
            detail: path.display().to_string(),
            prepared: PreparedWrite {
                path,
                content: content.to_string(),
                tool_call_id: None,
                expectation: WriteExpectation::Blind,
                reply_kind: WriteReplyKind::ProxyStructured,
                proxy_edit_count: 1,
            },
        };
        let (reply, receiver) = oneshot::channel();
        self.request_bridge_approval(ApprovalPrompt::proxy_write(
            spec,
            Some(invocation),
            None,
            reply,
        ));
        receiver
    }

    #[cfg(test)]
    pub(crate) fn reset_web_dispatch_count_for_test() {
        WEB_DISPATCH_TEST_COUNT.store(0, Ordering::SeqCst);
    }

    #[cfg(test)]
    pub(crate) fn web_dispatch_count_for_test() -> usize {
        WEB_DISPATCH_TEST_COUNT.load(Ordering::SeqCst)
    }

    #[cfg(test)]
    pub(crate) fn confirm_bridge_approval_for_test(&mut self, choice: ApprovalChoice) {
        self.confirm_bridge_approval(choice);
    }

    /// Confirms the front approval with the selected option.
    pub(crate) fn confirm_bridge_approval(&mut self, choice: ApprovalChoice) {
        if matches!(
            choice,
            ApprovalChoice::AllowPersistent
                | ApprovalChoice::AllowPersistentShort
                | ApprovalChoice::AllowPersistentPrefix(_)
                | ApprovalChoice::AllowPersistentPrefixShort(_)
        ) && let Some(prompt) = self.agents.approvals.front_mut()
            && prompt.confirming_allow != Some(choice)
        {
            prompt.confirming_allow = Some(choice);
            self.backend.status_message =
                Some(String::from("confirm bounded workspace allow rule"));
            return;
        }
        if choice == ApprovalChoice::DenyPersistent
            && let Some(prompt) = self.agents.approvals.front_mut()
            && !prompt.confirming_deny
        {
            prompt.confirming_deny = true;
            self.backend.status_message = Some(String::from("confirm workspace deny rule"));
            return;
        }
        let Some(prompt) = self.agents.approvals.pop_front() else {
            return;
        };
        self.resolve_approval(prompt, choice);
    }

    #[cfg(test)]
    pub(crate) fn cancel_rule_confirmation_for_test(&mut self) {
        self.cancel_rule_confirmation();
    }

    pub(crate) fn cancel_rule_confirmation(&mut self) {
        if let Some(prompt) = self.agents.approvals.front_mut() {
            prompt.confirming_deny = false;
            prompt.confirming_allow = None;
            self.backend.status_message = Some(String::from("trust rule confirmation cancelled"));
        }
    }

    #[cfg(test)]
    pub(crate) fn set_pre_write_verification_test_hook(
        &mut self,
        hook: impl FnOnce(&mut App) + Send + 'static,
    ) {
        self.write_verification_test_hooks.set_pre_verification(Box::new(hook));
    }

    #[cfg(test)]
    pub(super) fn run_pre_write_verification_test_hook(&mut self) {
        if let Some(hook) = self.write_verification_test_hooks.take_pre_verification() {
            hook(self);
        }
    }

    #[cfg(test)]
    pub(crate) fn set_post_write_test_hook(
        &mut self,
        hook: impl FnOnce(&mut App) + Send + 'static,
    ) {
        self.write_verification_test_hooks.set_post_write(Box::new(hook));
    }

    #[cfg(test)]
    pub(super) fn run_post_write_test_hook(&mut self) {
        if let Some(hook) = self.write_verification_test_hooks.take_post_write() {
            hook(self);
        }
    }

    /// Records the current revision after a test-controlled editor mutation.
    ///
    /// This captures the real buffer state at the same reduction boundary as a
    /// user edit; tests never construct `TurnObservation` values themselves.
    #[cfg(test)]
    pub(super) fn observe_post_write_test_revision(&self, session_id: &str, paths: &[PathBuf]) {
        if let Ok(revision) = self.evidence_revision_for_paths(paths) {
            self.observe_active_turn(session_id, TurnObservation::Revision { revision });
        }
    }
}

#[cfg(test)]
use super::approval::WebApprovalCall;
#[cfg(test)]
use super::prompt::ApprovalPrompt;
#[cfg(test)]
use crate::app::agents_mcp::ProxyRoute;
#[cfg(test)]
use ee_agent_host::ClientRequestResult;
#[cfg(test)]
use ee_agent_host::TurnObservation;
#[cfg(test)]
use ee_agent_protocol::CreateTerminalRequest;
#[cfg(test)]
use ee_agent_protocol::EnvVariable;
#[cfg(test)]
use ee_agent_protocol::SessionId;
#[cfg(test)]
use ee_agent_protocol::WriteTextFileRequest;
#[cfg(test)]
use std::collections::BTreeSet;
#[cfg(test)]
use tokio_util::sync::CancellationToken;

#[cfg(test)]
use super::approval::ProxyWriteSpec;
#[cfg(test)]
use tokio::sync::oneshot;
