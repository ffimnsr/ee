use super::*;

impl App {
    /// Opens the agents pane from non-command-line entry points such as
    /// `ee do agent shell`, preserving the same enabled/disabled behavior as
    /// `:agents`.
    pub(crate) fn open_agents_shell(&mut self) -> bool {
        self.dispatch_agents_command("agents", "")
    }

    /// Agents-mode command surface (Phase 0 inert path + Phase 3 pane).
    ///
    /// With the `agents` feature the full pane command set runs; without it
    /// every agents command reports a deterministic disabled message so
    /// keymaps and muscle memory stay stable across compile-time and runtime
    /// feature toggles.  Agent subprocesses are never started here when the
    /// feature or runtime config is off.
    pub(super) fn dispatch_agents_command(&mut self, head: &str, tail: &str) -> bool {
        #[cfg(feature = "agents")]
        return self.dispatch_agents_command_impl(head, tail);

        #[cfg(not(feature = "agents"))]
        {
            let _ = tail;
            let message = match head {
                "agents" => self.agents_status_message(),
                "agents_stop" | "agents_new" | "agents_threads" | "agents_clear"
                | "agents_close" | "agents_next" | "agents_prev" | "agents_layout"
                | "agents_thoughts" | "agents_mcp" => String::from("no active agent session"),
                _ => format!("unknown agents command: {head}"),
            };
            self.backend.status_message = Some(message);
            false
        }
    }

    #[cfg(feature = "agents")]
    pub(super) fn agents_status_message(&self) -> String {
        if self.config.agents.enabled {
            let (acp, mcp) = ee_agent_host::supported_protocol_versions();
            format!("agents mode enabled (ACP v{acp}, MCP {mcp})")
        } else {
            String::from("agents mode disabled (set `agents.enabled = true` to enable)")
        }
    }

    #[cfg(not(feature = "agents"))]
    fn agents_status_message(&self) -> String {
        String::from("agents mode disabled (compiled without `agents` feature)")
    }

    /// Feature-off stub: the pane never exists without `agents`.
    #[cfg(not(feature = "agents"))]
    pub(crate) fn agents_pane_open(&self) -> bool {
        false
    }

    /// Feature-off stub: thread focus is unreachable without `agents`.
    #[cfg(not(feature = "agents"))]
    #[allow(dead_code)]
    pub(crate) fn focus_thread(&mut self, _index: usize) {}
}
