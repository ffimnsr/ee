// CLI library shared by the `ee` binary, benches, and integration tests.
//
// Every module lives here exactly once: the binary is a thin wrapper over
// `entry::run_cli`, so a build codegens the editor once instead of compiling
// the same source tree for both the lib and bin targets.

// Shared crate-root items. Modules import what they need explicitly; nothing
// reaches submodules implicitly through `use super::*` beyond these constants.
use std::time::Duration;

// Constants shared through the prelude (`use super::*`) by several modules.
const INPUT_POLL_TIMEOUT: Duration = Duration::from_millis(16);
const MAX_INPUT_EVENTS_PER_TICK: usize = 128;
const FILE_PREVIEW_CHUNK_BYTES: u64 = 256 * 1024;
const RUNTIME_REPORT_READ_BYTES: u64 = 8 * 1024;
const EXIT_RUNTIME_CONFIG_MERGE: i32 = 2;
const EXIT_RUNTIME_GRAMMAR_SOURCE: i32 = 3;
const EXIT_RUNTIME_ASSET: i32 = 4;
const LONG_VERSION: &str = env!("EE_LONG_VERSION");

fn terminal_runtime_language_name(language: &str) -> String {
    match language.trim().to_ascii_lowercase().as_str() {
        "c#" => String::from("csharp"),
        "c++" => String::from("cpp"),
        normalized => normalized.to_string(),
    }
}

#[allow(dead_code)]
mod agent_bootstrap;
#[cfg(feature = "agents")]
#[allow(dead_code)]
mod agent_launch;
#[cfg(feature = "agents")]
mod agent_registry;
#[cfg(feature = "agents")]
mod agent_setup;
#[cfg(feature = "agents")]
mod agent_setup_prompt;
#[allow(dead_code)]
mod app;
mod args;
#[allow(dead_code)]
mod backend;
#[allow(dead_code)]
mod bench_alloc;
#[allow(dead_code)]
mod buffer;
#[allow(dead_code)]
mod clipboard;
#[allow(dead_code)]
mod config;
mod config_cmd;
mod doctor;
mod entry;
mod file_cmd;
#[allow(dead_code)]
mod folds;
#[allow(dead_code)]
mod git;
#[cfg(feature = "agents")]
#[allow(dead_code)]
mod harness_adopt_cmd;
#[cfg(feature = "agents")]
#[allow(dead_code)]
mod harness_cmd;
#[cfg(feature = "agents")]
#[allow(dead_code)]
mod harness_sweep_cmd;
#[allow(dead_code)]
mod highlight;
#[allow(dead_code)]
mod keymap;
mod language_cmd;
#[allow(dead_code)]
mod logs;
#[cfg(feature = "agents")]
#[allow(dead_code)]
mod mcp_cmd;
#[cfg(feature = "agents")]
mod mcp_setup;
#[allow(dead_code)]
mod mcp_trust;
pub mod perf;
#[allow(dead_code)]
mod picker;
#[allow(dead_code)]
pub(crate) mod plugin_ui;
mod plugins_cmd;
#[allow(dead_code)]
mod policy;
#[allow(dead_code)]
mod quickfix;
#[allow(dead_code)]
mod registers;
#[allow(dead_code)]
mod render_metrics;
mod runtime_cmd;
#[cfg(feature = "runtime-packs")]
mod runtime_pack;
mod schema_cmd;
#[allow(dead_code)]
mod secrets;
mod secrets_cmd;
#[cfg(feature = "agents")]
#[allow(dead_code)]
mod serve_cmd;
#[allow(dead_code)]
mod session;
#[cfg(feature = "agents")]
mod setup_prompt;
#[allow(dead_code)]
mod terminal;
#[allow(dead_code)]
mod text;
#[allow(dead_code)]
mod theme;
mod trust_cmd;
#[allow(dead_code)]
mod ui;
pub mod vlf_bench_support;
#[allow(dead_code)]
mod window;
#[allow(dead_code)]
mod workspace_trust;

#[cfg(test)]
mod tests;

pub use entry::run_cli as run;

#[cfg(test)]
pub(crate) use args::{
    ALL_AGENT_TRUST_PROFILES, AgentCommands, AgentTrustCommands, AgentTrustProfile, Cli, Commands,
    ConfigCommands, DoCommands, FileCommands, HarnessCommands, HarnessModeArg, LanguageCommands,
    McpCommands, McpTrustCommands, McpTrustTransport, PluginCommands, RuntimeCommands,
    SecretsCommands, WorkspaceTrustCommands,
};
#[cfg(test)]
pub(crate) use doctor::doctor_report;
#[cfg(test)]
pub(crate) use entry::{build_startup_app, coalesce_input_events, resolve_startup_launch};
#[cfg(test)]
pub(crate) use file_cmd::{count_file_line_feeds, read_file_head, read_file_tail};
#[cfg(test)]
pub(crate) use runtime_cmd::{
    EffectiveRuntimeLanguageRow, render_runtime_languages_report, render_runtime_report,
    runtime_report_exit_code,
};
#[cfg(test)]
pub(crate) use trust_cmd::{grant_agent_trust_profiles_at, revoke_agent_trust_profiles_at};
#[cfg(test)]
pub(crate) use ui::ui;
