//! CLI argument definitions (clap surface).
use super::*;

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
    /// Generate shell completion script
    Completions {
        /// Shell to generate completions for
        #[arg(value_enum)]
        shell: Shell,
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
    /// Manage host-local workspace trust grants
    Trust {
        #[command(subcommand)]
        command: AgentTrustCommands,
    },
}

#[derive(Debug, Subcommand)]
pub(crate) enum AgentTrustCommands {
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
    Agent,
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
        /// Trust ancestor `.ee.toml` runtime grammar overrides in current workspace
        #[arg(long, action = clap::ArgAction::SetTrue)]
        trust_workspace: bool,
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
