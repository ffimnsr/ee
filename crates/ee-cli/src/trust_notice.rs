//! Shared CLI notices for terminal-execution grants.

use crate::config::{TerminalSandboxMode, load_config};

/// Prints the sandbox policy line for one yolo grant. `scope` names the
/// affected route (native ACP v1 terminal or MCP terminal).
pub(crate) fn print_yolo_sandbox_notice(scope: &str) {
    let settings = load_config(None);
    let sandbox = &settings.agents.terminal_sandbox;
    if sandbox.mode == TerminalSandboxMode::Off {
        println!(
            "warning: --yolo is active while terminal sandbox mode is off; accepted {scope} commands run unsandboxed"
        );
    } else {
        println!(
            "--yolo {scope} commands run inside the terminal sandbox (mode={}, network={})",
            sandbox.mode.label(),
            sandbox.network.label()
        );
    }
}
