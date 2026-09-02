use super::*;

pub(super) fn state_after_turn_activity(current: ThreadUiState) -> ThreadUiState {
    match current {
        ThreadUiState::AwaitingPermission | ThreadUiState::AwaitingElicitation => current,
        _ => ThreadUiState::Running,
    }
}

impl App {
    /// Drains bridge requests, host events, and asynchronous replies.
    /// Called from the main loop on every tick; safe to call from tests.
    pub(crate) fn pump_agents(&mut self) {
        // Client requests can causally precede host events (for example URL
        // elicitation creation before elicitation completion). Present queued
        // requests first so completion events always find their UI state.
        self.pump_bridge_requests();

        let events = {
            let Some(host) = &mut self.agents.host else {
                return;
            };
            let mut events = Vec::new();
            while let Ok(event) = host.events.try_recv() {
                events.push(event);
            }
            events
        };
        for event in events {
            self.handle_agent_event(event);
        }

        self.pump_session_reply();
        self.pump_cancel_reply();
        self.pump_thread_action_reply();
        self.pump_external_critic_reply();
        self.pump_mcp_events();
        self.pump_mcp_replies();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn turn_activity_preserves_modal_states() {
        assert_eq!(
            state_after_turn_activity(ThreadUiState::AwaitingPermission),
            ThreadUiState::AwaitingPermission
        );
        assert_eq!(
            state_after_turn_activity(ThreadUiState::AwaitingElicitation),
            ThreadUiState::AwaitingElicitation
        );
        assert_eq!(state_after_turn_activity(ThreadUiState::Ready), ThreadUiState::Running);
    }
}
