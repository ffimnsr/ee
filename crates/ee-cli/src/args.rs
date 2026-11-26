//! CLI argument definitions (clap surface).
use std::path::PathBuf;

use clap::{Args, Parser, Subcommand, ValueEnum};
use clap_complete::Shell;

use super::*;
use crate::LONG_VERSION;

// ── CLI definition ────────────────────────────────────────────────────────────

#[derive(Debug, Parser)]
#[command(
    name = "ee",
    version,
    long_version = LONG_VERSION,
    about = "A terminal editor",
    long_about = None,
)]
pub(crate) struct Cli {
    /// Files to open (multiple allowed)
    #[arg(value_name = "FILE")]
    pub(crate) files: Vec<PathBuf>,

    /// Restore previously saved session state when no file args are passed
    #[arg(long)]
    pub(crate) restore_session: bool,

    /// Load a specific config file instead of layered defaults
    #[arg(long, value_name = "FILE")]
    pub(crate) config: Option<PathBuf>,

    /// Change the working directory before opening files
    #[arg(short = 'w', long, value_name = "DIR")]
    pub(crate) working_dir: Option<PathBuf>,

    #[command(subcommand)]
    pub(crate) command: Option<Commands>,

    /// Internal: run as the ee MCP proxy subprocess (spawned by ACP agents
    /// from the forwarded `mcpServers` config; never invoked by users).
    #[arg(long, hide = true)]
    pub(crate) mcp_proxy: bool,
}

#[derive(Debug, Subcommand)]
pub(crate) enum Commands {
    /// Run editor utility commands
    Do {
        #[command(subcommand)]
        command: DoCommands,
    },
}

#[derive(Debug, Subcommand)]
pub(crate) enum DoCommands {
    /// Check for problems and show config search precedence
    Doctor,
    /// Inspect and edit ee config files
    Config {
        #[command(subcommand)]
        command: ConfigCommands,
    },
    /// Open agent-facing UI entry points
    Agent {
        #[command(subcommand)]
        command: AgentCommands,
    },
    /// Manage host-local workspace trust for secret references and terminal execution
    /// (alias for `ee do agent trust workspace`)
    Trust {
        #[command(subcommand)]
        command: WorkspaceTrustCommands,
    },
    /// Manage host-local MCP server trust grants
    /// (alias for `ee do agent trust mcp`)
    Mcp {
        #[command(subcommand)]
        command: McpCommands,
    },
    /// List installed plugins from configured plugin directories
    Plugins {
        #[command(subcommand)]
        command: PluginCommands,
    },
    /// Show effective merged runtime languages
    Language {
        #[command(subcommand)]
        command: LanguageCommands,
    },
    /// Show runtime grammar and query resolution for a file or language
    Runtime {
        /// File to resolve through runtime language detection
        #[arg(long, value_name = "FILE")]
        file: Option<PathBuf>,
        /// Explicit language name to resolve before path/content detection
        #[arg(long, value_name = "LANGUAGE")]
        language: Option<String>,
        /// Resolve injected language identifier through `injection_regex`
        #[arg(long = "injection-language", value_name = "TEXT")]
        injection_language: Option<String>,
        #[command(subcommand)]
        command: Option<RuntimeCommands>,
    },
    /// Run file utility commands
    File {
        #[command(subcommand)]
        command: FileCommands,
    },
    /// Validate config file syntax and values
    Validate {
        /// Config file to validate
        #[arg(long, value_name = "FILE")]
        config: Option<PathBuf>,
    },
    /// Generate or check repository config schema
    Schema {
        #[command(subcommand)]
        command: SchemaCommands,
    },
    /// Generate or install shell completion script
    Completions {
        /// Shell to generate completions for
        #[arg(value_enum)]
        shell: Shell,
        /// Install the script in the shell's userspace config location
        /// instead of printing it (no sudo required)
        #[arg(long)]
        install: bool,
    },
    /// Manage the host-bound encrypted secrets store
    Secrets {
        #[command(subcommand)]
        command: SecretsCommands,
    },
}

#[derive(Debug, Subcommand)]
pub(crate) enum AgentCommands {
    /// Launch the editor directly into the agents TUI
    Shell,
    /// Manage host-local workspace and MCP trust grants
    Trust {
        #[command(subcommand)]
        command: AgentTrustCommands,
    },
    /// Discover and configure an installed ACP agent server
    Setup {
        /// Write agent server configuration to the user config layer instead of the workspace
        #[arg(long, action = clap::ArgAction::SetTrue)]
        user: bool,
    },
    /// Run the headless agent host without the editor
    Serve {
        /// Serve the remote JSON-RPC protocol over stdio (default transport)
        #[arg(long, action = clap::ArgAction::SetTrue)]
        stdio: bool,
        /// Serve over WebSocket + HTTP health/static on this address instead of stdio
        #[arg(long, value_name = "ADDR")]
        listen: Option<std::net::SocketAddr>,
        /// Bearer token file (0600) required for --listen; read per connection
        #[arg(long = "token-file", value_name = "FILE")]
        token_file: Option<PathBuf>,
        /// Static asset directory for the web client
        #[arg(long = "static-dir", value_name = "DIR")]
        static_dir: Option<PathBuf>,
        /// Allow a non-loopback --listen address (use an SSH tunnel or TLS proxy)
        #[arg(long = "allow-remote", action = clap::ArgAction::SetTrue)]
        allow_remote: bool,
        /// Workspace root the headless handler may touch; repeat for multiple roots
        #[arg(long = "allow-root", value_name = "DIR")]
        allow_roots: Vec<PathBuf>,
        /// Approval posture: ask (default), rules, or deny
        #[arg(long = "permission-mode", value_name = "MODE")]
        permission_mode: Option<String>,
        /// Append remote decisions to this audit log instead of the state dir
        #[arg(long = "audit-log", value_name = "FILE")]
        audit_log: Option<PathBuf>,
        /// Enable durable per-session history under this directory
        #[arg(long = "session-log-dir", value_name = "DIR")]
        session_log_dir: Option<PathBuf>,
    },
    /// Serve agent control tools to an MCP driver over stdio
    #[command(name = "serve-mcp")]
    ServeMcp {
        /// Workspace root the headless handler may touch; repeat for multiple roots
        #[arg(long = "allow-root", value_name = "DIR")]
        allow_roots: Vec<PathBuf>,
        /// Approval posture: ask (default), rules, or deny
        #[arg(long = "permission-mode", value_name = "MODE")]
        permission_mode: Option<String>,
        /// Append remote decisions to this audit log instead of the state dir
        #[arg(long = "audit-log", value_name = "FILE")]
        audit_log: Option<PathBuf>,
        /// Enable durable per-session history under this directory
        #[arg(long = "session-log-dir", value_name = "DIR")]
        session_log_dir: Option<PathBuf>,
    },
    /// Manage MCP server entries
    Mcp {
        #[command(subcommand)]
        command: AgentMcpCommands,
    },
    /// Recursive harness improvement loop
    Harness {
        #[command(subcommand)]
        command: HarnessCommands,
    },
    /// Inspect or prune durable agent session history
    Sessions {
        #[command(subcommand)]
        command: AgentSessionsCommands,
    },
}

/// `ee do agent sessions` subcommands.
#[derive(Debug, Subcommand)]
pub(crate) enum AgentSessionsCommands {
    /// Prune archived transcripts and provider session logs
    Prune(AgentSessionsPruneArgs),
}

/// `ee do agent sessions prune` options.
#[derive(Debug, Args)]
pub(crate) struct AgentSessionsPruneArgs {
    /// Remove entries not updated in this many days (default 30)
    #[arg(long = "older-than-days", default_value_t = 30, value_name = "DAYS")]
    pub(crate) older_than_days: u64,
    /// Remove every inactive entry regardless of age
    #[arg(long, action = clap::ArgAction::SetTrue)]
    pub(crate) all: bool,
    /// Report candidates without deleting anything
    #[arg(long, action = clap::ArgAction::SetTrue)]
    pub(crate) dry_run: bool,
}

/// `ee do agent harness` subcommands.
#[derive(Debug, Subcommand)]
pub(crate) enum HarnessCommands {
    /// Run one task through Propose -> Probe -> Compose -> Select
    Improve(Box<HarnessImproveArgs>),
    /// Apply a validated adopted harness tree to the workspace `.agents` surface
    Adopt(Box<HarnessAdoptArgs>),
    /// Run a JSON task manifest through per-domain ordered streams
    Sweep(Box<HarnessSweepArgs>),
    /// Run the deterministic demo loop with no model credentials
    Demo {
        /// Directory that receives the demo workspace and run root
        #[arg(long, value_name = "DIR")]
        root: Option<PathBuf>,
    },
}

/// `ee do agent harness adopt` options.
#[derive(Debug, Args)]
pub(crate) struct HarnessAdoptArgs {
    /// Run root whose `adopted/.agents` tree is applied
    #[arg(long = "run-root", value_name = "DIR", conflicts_with = "experience")]
    pub(crate) run_root: Option<PathBuf>,
    /// Experience entry `<domain>/<seq>` whose archived harness is applied
    #[arg(long = "experience", value_name = "DOMAIN/SEQ")]
    pub(crate) experience: Option<String>,
    /// Experience-library root used by --experience (defaults to the workspace library)
    #[arg(long = "library-root", value_name = "DIR")]
    pub(crate) library_root: Option<PathBuf>,
    /// Workspace whose `.agents` managed surface receives the tree
    #[arg(long, value_name = "DIR", default_value = ".")]
    pub(crate) workspace: PathBuf,
    /// Print the diff and write nothing
    #[arg(long = "dry-run", action = clap::ArgAction::SetTrue)]
    pub(crate) dry_run: bool,
    /// Apply without an interactive confirmation
    #[arg(long, action = clap::ArgAction::SetTrue)]
    pub(crate) yes: bool,
    /// Replace `.agents/memories` as part of the managed surface
    #[arg(long = "include-memories", action = clap::ArgAction::SetTrue)]
    pub(crate) include_memories: bool,
}

/// `ee do agent harness sweep` options.
#[derive(Debug, Args)]
pub(crate) struct HarnessSweepArgs {
    /// JSON task manifest: {"tasks":[{"id","prompt","domain","workspace"}]}
    #[arg(long, value_name = "FILE")]
    pub(crate) tasks: PathBuf,
    /// Recursion depth per task
    #[arg(long, default_value_t = 1)]
    pub(crate) k: usize,
    /// Candidate execution mode
    #[arg(long, value_enum, default_value_t = HarnessModeArg::Parallel)]
    pub(crate) mode: HarnessModeArg,
    /// Domain concurrency
    #[arg(long, default_value_t = 2)]
    pub(crate) workers: usize,
    /// Run directory (defaults to the ee state directory)
    #[arg(long = "run-root", value_name = "DIR")]
    pub(crate) run_root: Option<PathBuf>,
    /// Scratch experience-library root (defaults to <run-root>/library/<mode>-k<k>)
    #[arg(long = "library-root", value_name = "DIR")]
    pub(crate) library_root: Option<PathBuf>,
    /// Skip the reflector stage for every task
    #[arg(long = "no-reflect", action = clap::ArgAction::SetTrue)]
    pub(crate) no_reflect: bool,
    /// Print the plan without any model call
    #[arg(long = "dry-run", action = clap::ArgAction::SetTrue)]
    pub(crate) dry_run: bool,
}

/// `ee do agent harness improve` options.
#[derive(Debug, Args)]
pub(crate) struct HarnessImproveArgs {
    /// Workspace whose `.agents` tree is the base harness and whose files are the task source
    #[arg(long, value_name = "DIR", default_value = ".")]
    pub(crate) workspace: PathBuf,
    /// Task prompt handed to every rollout
    #[arg(long, value_name = "TEXT")]
    pub(crate) prompt: Option<String>,
    /// Read the task prompt from a file instead of --prompt
    #[arg(long = "prompt-file", value_name = "FILE")]
    pub(crate) prompt_file: Option<PathBuf>,
    /// Experience-library domain (defaults to the workspace directory name)
    #[arg(long, value_name = "NAME")]
    pub(crate) domain: Option<String>,
    /// Recursion depth; 1 is the single-step default
    #[arg(long, default_value_t = 1)]
    pub(crate) k: usize,
    /// Candidate execution mode
    #[arg(long, value_enum, default_value_t = HarnessModeArg::Parallel)]
    pub(crate) mode: HarnessModeArg,
    /// Domain concurrency for stream runs
    #[arg(long, default_value_t = 2)]
    pub(crate) workers: usize,
    /// Print the plan without any model call
    #[arg(long = "dry-run", action = clap::ArgAction::SetTrue)]
    pub(crate) dry_run: bool,
    /// Run directory (defaults to the ee state directory)
    #[arg(long = "run-root", value_name = "DIR")]
    pub(crate) run_root: Option<PathBuf>,
    /// Scratch experience-library root for measurement runs
    #[arg(long = "library-root", value_name = "DIR")]
    pub(crate) library_root: Option<PathBuf>,
    /// Improver model override; defaults to the configured agent default
    #[arg(long = "improver-model", value_name = "ID")]
    pub(crate) improver_model: Option<String>,
    /// Improver reasoning-effort label
    #[arg(long = "improver-effort", value_name = "LABEL")]
    pub(crate) improver_effort: Option<String>,
    /// Maximum improver model steps per session
    #[arg(long = "improver-steps", default_value_t = 12)]
    pub(crate) improver_steps: usize,
    /// Improver wall-clock timeout in seconds
    #[arg(long = "improver-timeout", default_value_t = 300)]
    pub(crate) improver_timeout: u64,
    /// Reflection persistence: accept writes the experience entry, decline runs
    /// the reflector without writing
    #[arg(long = "reflection", value_enum, default_value_t = ReflectionChoiceArg::Accept)]
    pub(crate) reflection: ReflectionChoiceArg,
    /// Skip the reflector stage entirely
    #[arg(long = "no-reflect", action = clap::ArgAction::SetTrue)]
    pub(crate) no_reflect: bool,
}

/// Reflection persistence choice on the command line.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub(crate) enum ReflectionChoiceArg {
    /// Write the experience entry as part of this invocation
    Accept,
    /// Run reflection but write nothing
    Decline,
}

/// Candidate execution mode on the command line.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub(crate) enum HarnessModeArg {
    /// Independent candidates against the base harness
    Parallel,
    /// Chained steps; each composition becomes the next base
    Sequential,
}

/// `ee do agent mcp` subcommands for server config management.
#[derive(Debug, Subcommand)]
pub(crate) enum AgentMcpCommands {
    /// Add or replace one MCP server entry
    Add(AgentMcpAddArgs),
    /// List configured MCP servers
    List(AgentMcpListArgs),
    /// Remove one MCP server entry
    Rm(AgentMcpRemoveArgs),
}

/// `ee do agent mcp add` options.
#[derive(Debug, Args)]
pub(crate) struct AgentMcpAddArgs {
    /// MCP server id
    #[arg(long, value_name = "ID")]
    pub(crate) id: String,
    /// stdio executable to launch
    #[arg(long, value_name = "COMMAND", conflicts_with = "url")]
    pub(crate) command: Option<String>,
    /// stdio argument; repeat as needed
    #[arg(long = "arg", value_name = "ARG")]
    pub(crate) args: Vec<String>,
    /// stdio environment variable as KEY=VALUE; repeat. Secret values must be secret:// references
    #[arg(long = "env", value_name = "KEY=VALUE")]
    pub(crate) env: Vec<String>,
    /// stdio working directory
    #[arg(long, value_name = "DIR", conflicts_with = "url")]
    pub(crate) cwd: Option<PathBuf>,
    /// Streamable HTTP endpoint URL
    #[arg(long, value_name = "URL", conflicts_with = "command")]
    pub(crate) url: Option<String>,
    /// Streamable HTTP header as KEY=VALUE; repeat. Secret values must be secret:// references
    #[arg(long = "header", value_name = "KEY=VALUE")]
    pub(crate) headers: Vec<String>,
    /// Streamable HTTP request timeout in milliseconds
    #[arg(long = "timeout-ms", value_name = "MS")]
    pub(crate) timeout_ms: Option<u64>,
    /// Write to the user config layer instead of the workspace layer
    #[arg(long, action = clap::ArgAction::SetTrue)]
    pub(crate) user: bool,
}

/// `ee do agent mcp list` options.
#[derive(Debug, Args)]
pub(crate) struct AgentMcpListArgs {
    /// List only the user config layer instead of the merged effective config
    #[arg(long, action = clap::ArgAction::SetTrue)]
    pub(crate) user: bool,
}

/// `ee do agent mcp rm` options.
#[derive(Debug, Args)]
pub(crate) struct AgentMcpRemoveArgs {
    /// MCP server id to remove
    #[arg(long, value_name = "ID")]
    pub(crate) id: String,
    /// Remove from the user config layer instead of the workspace layer
    #[arg(long, action = clap::ArgAction::SetTrue)]
    pub(crate) user: bool,
}

#[derive(Debug, Subcommand)]
pub(crate) enum AgentTrustCommands {
    /// Manage workspace trust and terminal execution grants
    Workspace {
        #[command(subcommand)]
        command: WorkspaceTrustCommands,
    },
    /// Manage host-local MCP server trust grants
    Mcp {
        #[command(subcommand)]
        command: McpTrustCommands,
    },
    /// Grant all built-in trust profiles, or only profiles named with --profile
    Grant {
        /// Built-in host-local profile to grant; repeat to grant multiple profiles
        #[arg(long = "profile", value_enum)]
        profiles: Vec<AgentTrustProfile>,
    },
    /// Revoke one or more built-in host-local trust profiles
    Revoke {
        /// Built-in host-local profile to revoke; repeat to revoke multiple profiles
        #[arg(long = "profile", value_enum, required = true)]
        profiles: Vec<AgentTrustProfile>,
    },
}

#[derive(Debug, Subcommand)]
pub(crate) enum McpTrustCommands {
    /// Grant trust for one MCP server's pinned tools
    Grant {
        /// MCP server id to trust; required, no implicit default
        #[arg(long, value_name = "SERVER")]
        server: String,
        /// Transport to grant; defaults to both stdio and ACP for this server
        #[arg(long, value_enum, default_value = "all")]
        transport: McpTrustTransport,
        /// Also grant terminal execution (`ee_terminal_create`) for this server
        #[arg(long)]
        execute: bool,
    },
    /// Revoke trust for one MCP server
    Revoke {
        /// MCP server id to revoke; required, no implicit default
        #[arg(long, value_name = "SERVER")]
        server: String,
        /// Transport to revoke; defaults to both stdio and ACP for this server
        #[arg(long, value_enum, default_value = "all")]
        transport: McpTrustTransport,
        /// Revoke only the terminal-execution opt-in, keeping the other tools
        #[arg(long)]
        execute: bool,
    },
    /// Print recorded MCP server trust grants
    Status {
        /// Restrict output to one server id
        #[arg(long, value_name = "SERVER")]
        server: Option<String>,
    },
}

/// Transport selector for MCP server trust commands.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub(crate) enum McpTrustTransport {
    Stdio,
    Acp,
    All,
}

#[derive(Debug, Subcommand)]
pub(crate) enum McpCommands {
    /// Grant, revoke, or inspect MCP server trust
    Trust {
        #[command(subcommand)]
        command: McpTrustCommands,
    },
}

/// Workspace trust decision for the current directory.
#[derive(Debug, Subcommand)]
pub(crate) enum WorkspaceTrustCommands {
    /// Trust the current workspace and allow its secret references
    Grant {
        /// Also grant prompt-free native terminal execution for this workspace
        #[arg(long)]
        execute: bool,
        /// Grant unrestricted native terminal execution: any command text,
        /// including pipes, redirects, and globs. Requires the workspace to be
        /// trusted; commands still respect read-only agent modes, explicit
        /// denies, mandatory confirms, safeguards, and the terminal sandbox
        #[arg(long)]
        yolo: bool,
    },
    /// Record the current workspace as untrusted and block its secret references
    Revoke {
        /// Revoke only terminal execution, keeping the workspace trust decision
        #[arg(long)]
        execute: bool,
    },
    /// Print the recorded workspace trust decision
    Status,
}

/// Application-owned trust profiles exposed by the CLI. Project configuration
/// cannot add values to this enum or grant their authority.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub(crate) enum AgentTrustProfile {
    /// Bounded ee MCP read tools on stdio and ACP routes
    #[value(name = "mcp_safe_read")]
    McpSafeRead,
    /// Exact workspace-root Git read commands
    #[value(name = "git_readonly")]
    GitReadonly,
    /// Bounded workspace-root terminal inspection commands
    #[value(name = "terminal_readonly")]
    TerminalReadonly,
}

pub(crate) const ALL_AGENT_TRUST_PROFILES: &[AgentTrustProfile] = &[
    AgentTrustProfile::McpSafeRead,
    AgentTrustProfile::GitReadonly,
    AgentTrustProfile::TerminalReadonly,
];

impl AgentTrustProfile {
    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::McpSafeRead => "mcp_safe_read",
            Self::GitReadonly => "git_readonly",
            Self::TerminalReadonly => "terminal_readonly",
        }
    }
}

#[derive(Debug, Subcommand)]
pub(crate) enum LanguageCommands {
    /// List effective merged runtime languages after layered config merge
    List {
        /// Directory whose ancestor `.ee.toml` layers should participate in merge
        #[arg(long, value_name = "DIR", conflicts_with = "file")]
        dir: Option<PathBuf>,
        /// File whose ancestor `.ee.toml` layers should participate in merge
        #[arg(long, value_name = "FILE", conflicts_with = "dir")]
        file: Option<PathBuf>,
    },
}

#[derive(Debug, Subcommand)]
pub(crate) enum PluginCommands {
    /// List installed plugins discovered from bundled and user plugin directories
    #[command(visible_alias = "ls")]
    List,
    /// Validate a v2 plugin manifest file or plugin directory
    Validate {
        /// Manifest file or plugin directory; defaults to every configured root
        #[arg(value_name = "PATH")]
        path: Option<PathBuf>,
    },
    /// Write the generated plugin manifest JSON Schema
    Schema {
        /// Schema output path
        #[arg(long, default_value = "schemas/plugin-manifest-v2.json", value_name = "FILE")]
        output: PathBuf,
    },
}

#[derive(Debug, Args)]
#[group(required = true, multiple = false)]
pub(crate) struct ConfigScopeArgs {
    /// Use user XDG config at ~/.config/ee/config.toml
    #[arg(long, action = clap::ArgAction::SetTrue)]
    pub(crate) global: bool,
    /// Use config in current directory at ./.ee.toml
    #[arg(long, action = clap::ArgAction::SetTrue)]
    pub(crate) local: bool,
}

impl ConfigScopeArgs {
    pub(crate) fn scope(&self) -> config::ConfigScope {
        if self.global { config::ConfigScope::Global } else { config::ConfigScope::Local }
    }
}

#[derive(Debug, Subcommand)]
pub(crate) enum ConfigCommands {
    /// Run interactive configuration wizards
    Setup {
        #[command(subcommand)]
        command: ConfigSetupCommands,
    },
    /// Create a fully commented config template; default target is ./.ee.toml
    Init {
        /// Create user XDG config at ~/.config/ee/config.toml
        #[arg(long, action = clap::ArgAction::SetTrue)]
        global: bool,
    },
    /// Show fully merged effective config for current directory
    Show,
    /// Read one key from chosen config file
    Get {
        #[command(flatten)]
        scope: ConfigScopeArgs,
        /// Dotted config key path, e.g. lsp.servers.rust.command
        key: String,
    },
    /// Set one key in chosen config file
    Set {
        #[command(flatten)]
        scope: ConfigScopeArgs,
        /// Dotted config key path, e.g. wrap_lines or lsp.servers.rust.command
        key: String,
        /// TOML value literal. Bare words become strings.
        value: String,
    },
}

#[derive(Debug, Subcommand)]
pub(crate) enum ConfigSetupCommands {
    /// Discover and configure an installed ACP agent server
    Agent {
        /// Write agent server configuration to the user config layer instead of the workspace
        #[arg(long, action = clap::ArgAction::SetTrue)]
        user: bool,
    },
    /// Add or remove MCP servers interactively
    Mcp {
        /// Write MCP server configuration to the user config layer instead of the workspace
        #[arg(long, action = clap::ArgAction::SetTrue)]
        user: bool,
    },
}

#[derive(Debug, Subcommand)]
pub(crate) enum RuntimeCommands {
    /// Show effective merged runtime languages after layered merge
    Languages,
    /// Materialize pinned grammar sources from the cargo registry
    Fetch {
        /// Fetch all configured runtime grammars
        #[arg(long, action = clap::ArgAction::SetTrue)]
        all: bool,
        /// Fetch only selected runtime languages
        #[arg(long = "language", value_name = "LANGUAGE")]
        languages: Vec<String>,
        /// Directory used to stage grammar source trees
        #[arg(long, value_name = "DIR")]
        source_root: Option<PathBuf>,
        /// Replace any existing staged source trees
        #[arg(long, action = clap::ArgAction::SetTrue)]
        force: bool,
        /// Print staged source directories instead of short references
        #[arg(short, long)]
        verbose: bool,
        /// Trust ancestor `.ee.toml` runtime grammar overrides in current workspace
        #[arg(long, action = clap::ArgAction::SetTrue)]
        trust_workspace: bool,
    },
    /// Build runtime grammar libraries and query assets
    Build {
        /// Build all configured runtime grammars
        #[arg(long, action = clap::ArgAction::SetTrue)]
        all: bool,
        /// Build only selected runtime languages
        #[arg(long = "language", value_name = "LANGUAGE")]
        languages: Vec<String>,
        /// Directory used to stage grammar source trees
        #[arg(long, value_name = "DIR")]
        source_root: Option<PathBuf>,
        /// Directory that will receive `grammars/` and `queries/`
        #[arg(long, value_name = "DIR")]
        output_root: Option<PathBuf>,
        /// Replace existing grammar libraries before rebuilding
        #[arg(long, action = clap::ArgAction::SetTrue)]
        force: bool,
        /// Skip host-side dynamic library load validation after compile
        #[arg(long, action = clap::ArgAction::SetTrue)]
        skip_load: bool,
        /// Exit non-zero when any selected grammar fails to fetch or build
        #[arg(long, action = clap::ArgAction::SetTrue)]
        strict: bool,
        /// Print full output paths instead of short references
        #[arg(short, long)]
        verbose: bool,
        /// Trust ancestor `.ee.toml` runtime grammar overrides in current workspace
        #[arg(long, action = clap::ArgAction::SetTrue)]
        trust_workspace: bool,
    },
    /// Install prebuilt grammar assets for selected languages
    Install {
        /// Install every configured runtime language
        #[arg(long, action = clap::ArgAction::SetTrue)]
        all: bool,
        /// Install only selected runtime languages
        #[arg(value_name = "LANGUAGE")]
        languages: Vec<String>,
        /// Install from a local language pack instead of downloading one
        #[arg(long, value_name = "FILE")]
        pack: Option<PathBuf>,
        /// Expected SHA-256 of the language pack
        #[arg(long = "sha256", value_name = "HEX")]
        sha256: Option<String>,
        /// Re-download and overwrite existing grammar libraries
        #[arg(long, action = clap::ArgAction::SetTrue)]
        force: bool,
        /// Print installed file paths
        #[arg(short, long)]
        verbose: bool,
    },
}

#[derive(Debug, Subcommand)]
pub(crate) enum FileCommands {
    /// Count line-feed bytes in a file like `wc -l`
    LineCheck {
        /// File to inspect
        #[arg(value_name = "FILE")]
        file: PathBuf,
    },
    /// Print first lines of a file like `head`
    Head {
        /// Number of lines to print
        #[arg(short = 'n', long = "lines", default_value_t = 10, value_name = "LINES")]
        lines: usize,
        /// File to inspect
        #[arg(value_name = "FILE")]
        file: PathBuf,
    },
    /// Print last lines of a file like `tail`
    Tail {
        /// Number of lines to print
        #[arg(short = 'n', long = "lines", default_value_t = 10, value_name = "LINES")]
        lines: usize,
        /// File to inspect
        #[arg(value_name = "FILE")]
        file: PathBuf,
    },
}

#[derive(Debug, Subcommand)]
pub(crate) enum SchemaCommands {
    /// Write generated config JSON Schema to schemas/
    Generate {
        /// Schema output path
        #[arg(long, default_value = "schemas/ee-config.schema.json", value_name = "FILE")]
        output: PathBuf,
    },
    /// Fail when checked-in config schema differs from generated output
    Check {
        /// Schema path to compare against generated output
        #[arg(long, default_value = "schemas/ee-config.schema.json", value_name = "FILE")]
        schema: PathBuf,
    },
}

#[derive(Debug, Subcommand)]
pub(crate) enum SecretsCommands {
    /// Create or replace a stored secret value
    Set {
        /// Canonical secret name (ASCII letters, digits, `.`, `_`, `-`)
        #[arg(value_name = "NAME")]
        name: String,
        /// Read the secret value from standard input instead of the hidden terminal prompt
        #[arg(long, action = clap::ArgAction::SetTrue)]
        stdin: bool,
    },
    /// Print a stored secret value
    Get {
        /// Canonical secret name
        #[arg(value_name = "NAME")]
        name: String,
        /// Allow printing the secret value to a terminal (never use on shared displays)
        #[arg(long, action = clap::ArgAction::SetTrue)]
        force: bool,
    },
    /// List stored secret names
    List,
    /// Delete a stored secret
    Delete {
        /// Canonical secret name
        #[arg(value_name = "NAME")]
        name: String,
    },
    /// Remove encrypted vault file; preserves OS-keychain key for a fresh future vault
    Reset,
    /// Report safe secrets-store state
    Status,
}
