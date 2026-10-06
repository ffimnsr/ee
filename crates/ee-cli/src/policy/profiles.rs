//! Curated command validation profiles.
//!
//! The profile registry is application-owned and versioned: stable profile
//! ids (`git_readonly`, `terminal_readonly`, `rust_validate`, `safe_read`)
//! map to fixed structured executable/argv entries, each bound to the
//! workspace cwd, a timeout cap, an output cap, and the execute category.
//! Config stores profile ids only and can never mutate the registry; unknown
//! ids and versions fail closed.
//!
//! Trailing arguments are controlled by [`ProfileArgPolicy`]:
//! - `git_readonly`, `terminal_readonly`, and `rust_validate` entries are
//!   exact (`ProfileArgPolicy::EXACT`).
//! - `safe_read` entries allow a curated flag allowlist, optional value flags
//!   (which consume one opaque value token), at most one opaque pattern token,
//!   and workspace-relative path operands. The approval layer validates every
//!   path operand against the canonical workspace before an entry can match.

use std::time::Duration;

/// Version of the application-owned profile registry.
pub(crate) const PROFILE_REGISTRY_VERSION: u64 = 3;

/// Built-in terminal profile for safe direct workspace inspection. `pwd` and
/// `ls` are exact entries; the bridge additionally accepts one path-validated
/// `cat` operand only after canonical workspace and protected-path checks.
pub(crate) const TERMINAL_READONLY_PROFILE: &str = "terminal_readonly";

/// Built-in read-only command allowlist evaluated as a built-in allow (no
/// prompt, no persistent rule, no use budget) only while the host-local
/// workspace trust decision is `trusted`. Entries never include mutators,
/// package installs, interpreters, network clients, secret-dumping tools, or
/// shell wrappers.
pub(crate) const SAFE_READ_PROFILE: &str = "safe_read";

/// Built-in MCP profile for bounded, read-only ee tools. This profile is
/// application-owned: adding a manifest read tool never expands a persisted
/// grant until this list is intentionally updated.
pub(crate) const EE_MCP_SAFE_READ_PROFILE: &str = "ee_mcp_safe_read";
/// Manifest schema version accepted by the built-in safe-read profile.
pub(crate) const EE_MCP_SAFE_READ_TOOL_SCHEMA_VERSION: u64 = 1;

/// Fixed tools covered by [`EE_MCP_SAFE_READ_PROFILE`]. Write, execute, and
/// unknown tools must never appear here.
pub(crate) const EE_MCP_SAFE_READ_TOOLS: &[&str] = &[
    "ee_workspace_roots",
    "ee_list_directory",
    "ee_list_directory_all",
    "ee_search_files",
    "ee_search_files_all",
    "ee_search_text",
    "ee_search_text_regex",
    "ee_search_text_in_files",
    "ee_read_buffer",
    "ee_read_buffer_lines",
    "ee_open_buffers",
    "ee_get_diagnostics",
    "ee_get_file_diagnostics",
    "ee_document_symbols",
    "ee_references",
    "ee_list_code_actions",
    "ee_preview_rename_symbol",
    "ee_read_text_file",
    "ee_terminal_output",
    "ee_terminal_output_since",
    "ee_terminal_wait",
    "ee_terminal_wait_long",
    "ee_git_status",
    "ee_git_diff",
    "ee_git_diff_staged",
    "ee_git_diff_file",
    "ee_changed_files",
    "ee_review_context",
    "ee_tools_manifest",
    "ee_project_instructions",
    "ee_read_notes",
    "ee_read_note",
    "ee_file_dependency_map",
    "ee_symbol_dependency_map",
    "ee_diagnostics",
];

/// Whether `profile` and `tool` form a fixed, application-owned MCP read
/// profile entry. Unknown profiles and non-read manifest tools fail closed.
pub(crate) fn mcp_read_profile_matches(profile: &str, tool: &str) -> bool {
    profile == EE_MCP_SAFE_READ_PROFILE && EE_MCP_SAFE_READ_TOOLS.contains(&tool)
}

/// Whether `profile` identifies an application-owned MCP read profile.
pub(crate) fn is_known_mcp_read_profile(profile: &str) -> bool {
    profile == EE_MCP_SAFE_READ_PROFILE
}

/// Whether `profile` is the built-in workspace-trusted read-only allowlist.
pub(crate) fn is_safe_read_profile(profile: &str) -> bool {
    profile == SAFE_READ_PROFILE
}

/// Trailing-argument policy for one fixed structured entry.
///
/// Matching walks tokens after the entry argv prefix:
/// - a `-`-prefixed token matches only when listed in `flags` or
///   `value_flags`;
/// - a `value_flags` token consumes the next token as an opaque value (this
///   is how `-name`, `-type`, `--since`, `-n`, … receive their operands);
/// - otherwise, with `allow_pattern`, the first such token is an opaque
///   pattern (`grep`, `rg`, `fd`); a pattern-only entry (`allow_paths`
///   false) accepts every remaining non-flag token as free text (`echo`,
///   `printf`, `which`, `ps`);
/// - otherwise, with `allow_paths`, the token is a path operand the approval
///   layer validates against the canonical workspace.
///
/// Anything else rejects the entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ProfileArgPolicy {
    pub(crate) flags: &'static [&'static str],
    pub(crate) value_flags: &'static [&'static str],
    pub(crate) allow_pattern: bool,
    pub(crate) allow_paths: bool,
}

impl ProfileArgPolicy {
    /// Exact structured argv: no trailing tokens of any kind.
    pub(crate) const EXACT: Self =
        Self { flags: &[], value_flags: &[], allow_pattern: false, allow_paths: false };
}

/// One fixed structured entry inside a curated profile.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ProfileEntry {
    pub(crate) executable: &'static str,
    pub(crate) argv: &'static [&'static str],
    /// Trailing-argument policy (registry metadata).
    pub(crate) args: ProfileArgPolicy,
    /// Bound on one profile command run (registry metadata; the terminal
    /// pipeline caps and cancellation paths are unchanged).
    pub(crate) timeout_cap: Duration,
    /// Bound on retained output for one profile command.
    pub(crate) output_cap: usize,
}

/// One curated profile: stable id plus fixed structured entries.
#[derive(Debug, Clone, Copy)]
pub(crate) struct CuratedProfile {
    pub(crate) id: &'static str,
    pub(crate) entries: &'static [ProfileEntry],
}

/// One matched profile entry plus the path operands the approval layer must
/// validate against the canonical workspace.
#[derive(Debug)]
pub(crate) struct ProfileMatch<'a> {
    pub(crate) profile: &'static str,
    pub(crate) entry: &'static ProfileEntry,
    pub(crate) path_operands: Vec<&'a str>,
}

// ── safe_read flag allowlists ────────────────────────────────────────────────

const LS_FLAGS: &[&str] = &[
    "-a",
    "-A",
    "-l",
    "-h",
    "-1",
    "-r",
    "-t",
    "-S",
    "-d",
    "-F",
    "-G",
    "-la",
    "-al",
    "-lh",
    "-lha",
    "-lah",
    "-lt",
    "-ltr",
    "-alg",
    "--color=never",
    "--no-color",
];
const READ_FLAGS: &[&str] = &["-q", "-v", "--quiet", "-n", "-#"];
const HEAD_FLAGS: &[&str] = &["-q", "-v", "-z"];
const HEAD_VALUE_FLAGS: &[&str] = &["-n", "--lines", "-c", "--bytes"];
const WC_FLAGS: &[&str] = &["-l", "-w", "-c", "-m", "-L", "--lines", "--words", "--bytes"];
const STAT_VALUE_FLAGS: &[&str] = &["-c", "--format", "-f", "--file-system"];
const FILE_FLAGS: &[&str] = &["-b", "-i", "-L", "-s", "--mime", "--mime-type", "--brief"];
const DU_FLAGS: &[&str] = &["-h", "-s", "-a", "-c", "--apparent-size"];
const DU_VALUE_FLAGS: &[&str] = &["-d", "--max-depth"];
const TREE_FLAGS: &[&str] = &["-a", "-d", "-f", "-i", "--noreport", "-L"];
const TREE_VALUE_FLAGS: &[&str] = &["-L"];
const FIND_FLAGS: &[&str] = &[
    "-name",
    "-iname",
    "-type",
    "-maxdepth",
    "-mindepth",
    "-path",
    "-ipath",
    "-mtime",
    "-size",
    "-print",
    "-print0",
    "-not",
    "-o",
    "-a",
    "-and",
    "-or",
    "-empty",
    "-readable",
    "-writable",
    "-executable",
];
const FIND_VALUE_FLAGS: &[&str] =
    &["-name", "-iname", "-type", "-maxdepth", "-mindepth", "-path", "-ipath", "-mtime", "-size"];
const FD_FLAGS: &[&str] =
    &["-s", "--case-sensitive", "-i", "--ignore-case", "-F", "--fixed-strings", "--absolute-path"];
const FD_VALUE_FLAGS: &[&str] =
    &["-e", "--extension", "-t", "--type", "-d", "--max-depth", "-g", "--glob"];
const SORT_FLAGS: &[&str] = &["-n", "-r", "-u", "-f", "-h", "-V", "-z", "-b", "-s"];
const SORT_VALUE_FLAGS: &[&str] = &["-k", "-t"];
const CUT_VALUE_FLAGS: &[&str] = &["-d", "-f", "-c", "-b", "--output-delimiter"];
const CUT_FLAGS: &[&str] = &["-s", "--complement", "--only-delimited"];
const COMM_FLAGS: &[&str] = &["-1", "-2", "-3", "--check-order", "--nocheck-order"];
const DIFF_FLAGS: &[&str] = &["-q", "-s", "-u", "-r", "-N", "-w", "-b", "--brief", "--unified"];
const CHECKSUM_FLAGS: &[&str] = &["-b", "-t", "--binary", "--text"];
// Recursion is intentionally absent: recursive `grep` does not honor ignore
// rules, so it would read hidden and ignored workspace files that rg (default
// ignore semantics) and validated explicit operands do not reach.
const GREP_FLAGS: &[&str] = &[
    "-n",
    "-i",
    "-I",
    "-l",
    "-L",
    "-w",
    "-x",
    "-v",
    "-c",
    "-o",
    "-E",
    "-F",
    "-G",
    "-s",
    "-q",
    "-H",
    "-h",
    "--color=never",
    "--no-color",
    "--line-number",
];
const GREP_VALUE_FLAGS: &[&str] =
    &["-e", "-A", "-B", "-C", "-m", "--max-count", "--include", "--exclude", "--exclude-dir"];
// Hidden/ignored-file and symlink-following flags are intentionally absent:
// the default rg traversal honors ignore rules and keeps the reach of one
// built-in allow at or below validated explicit-path inspection.
const RG_FLAGS: &[&str] = &[
    "-n",
    "-i",
    "-S",
    "-s",
    "-F",
    "-w",
    "-x",
    "-v",
    "-c",
    "-l",
    "-o",
    "-U",
    "--files",
    "--no-color",
    "--color=never",
    "--line-number",
    "--column",
    "--with-filename",
    "--no-filename",
];
const RG_VALUE_FLAGS: &[&str] =
    &["-g", "--glob", "-t", "--type", "-T", "--type-not", "-m", "--max-count", "-A", "-B", "-C"];
const GIT_DIFF_FLAGS: &[&str] = &[
    "--cached",
    "--staged",
    "--stat",
    "--numstat",
    "--shortstat",
    "--name-only",
    "--name-status",
    "--summary",
    "--check",
    "--patch",
    "--no-color",
    "--color=never",
    "--relative",
    "--quiet",
    "--exit-code",
    "--ignore-all-space",
    "--ignore-space-change",
    "-w",
    "-b",
    "-p",
    "-u",
    "-M",
    "-C",
    "--find-renames",
    "--find-copies",
];
const GIT_DIFF_VALUE_FLAGS: &[&str] = &["-U", "--unified", "--diff-filter"];
const GIT_LOG_FLAGS: &[&str] = &[
    "--oneline",
    "--graph",
    "--decorate",
    "--stat",
    "--name-only",
    "--name-status",
    "--all",
    "--no-color",
    "--color=never",
    "--follow",
    "--reverse",
    "--merges",
    "--no-merges",
    "--patch",
    "--no-patch",
    "-p",
];
const GIT_LOG_VALUE_FLAGS: &[&str] =
    &["-n", "--max-count", "--since", "--until", "--author", "--grep", "--format"];
const GIT_SHOW_FLAGS: &[&str] = &[
    "--stat",
    "--name-only",
    "--name-status",
    "--oneline",
    "--no-color",
    "--color=never",
    "--patch",
    "--no-patch",
    "--summary",
    "-s",
    "-p",
];
const GIT_SHOW_VALUE_FLAGS: &[&str] = &["--format"];
const GIT_BLAME_FLAGS: &[&str] =
    &["-w", "--line-porcelain", "--porcelain", "--no-color", "--color=never", "-s"];
const GIT_BLAME_VALUE_FLAGS: &[&str] = &["-L", "--date"];
const GIT_LS_FILES_FLAGS: &[&str] = &[
    "--cached",
    "--deleted",
    "--modified",
    "--others",
    "--ignored",
    "--stage",
    "--unmerged",
    "-s",
    "-z",
    "--error-unmatch",
    "--exclude-standard",
];
const GIT_LS_TREE_FLAGS: &[&str] = &["-r", "-t", "-d", "--name-only", "--long", "-z"];
const GIT_REV_PARSE_FLAGS: &[&str] = &[
    "--verify",
    "--short",
    "--abbrev-ref",
    "--show-toplevel",
    "--git-dir",
    "--is-inside-work-tree",
    "--is-bare-repository",
];
const GIT_DESCRIBE_FLAGS: &[&str] =
    &["--tags", "--always", "--long", "--dirty", "--all", "--first-parent"];
const GIT_SHORTLOG_FLAGS: &[&str] = &["-s", "-e", "--no-merges", "--all"];
const GIT_SHORTLOG_VALUE_FLAGS: &[&str] = &["-n"];
const GIT_NAME_REV_FLAGS: &[&str] = &["--all", "--tags", "--name-only"];
const GIT_GREP_FLAGS: &[&str] = &[
    "-n",
    "-i",
    "-I",
    "-l",
    "-L",
    "-w",
    "-c",
    "-o",
    "-E",
    "-F",
    "-P",
    "--cached",
    "--untracked",
    "--no-color",
    "--color=never",
];
const GIT_GREP_VALUE_FLAGS: &[&str] = &["-e"];
const GIT_CHECK_IGNORE_FLAGS: &[&str] = &["-v", "-q"];
const GIT_BRANCH_FLAGS: &[&str] =
    &["--all", "-a", "--remotes", "-r", "-v", "-vv", "--no-color", "--color=never"];
const GIT_TAG_FLAGS: &[&str] = &["-l", "--list", "--no-color"];
const GIT_TAG_VALUE_FLAGS: &[&str] = &["-n"];
const NPM_LIST_FLAGS: &[&str] = &[
    "--depth",
    "--json",
    "--long",
    "--parseable",
    "--prod",
    "--production",
    "--dev",
    "--all",
    "--global",
    "-g",
    "--silent",
];
const NPM_LIST_VALUE_FLAGS: &[&str] = &["--depth"];
const PS_FLAGS: &[&str] = &["-e", "-f", "-a", "-u", "-x", "--forest", "--no-headers"];
const DF_FLAGS: &[&str] = &["-h", "-i", "-T", "--total", "-P"];

/// The application-owned curated profile registry.
///
/// `safe_read` is listed first so workspace-trusted read-only inspection wins
/// over grant-required exact profiles for the same executable. Entries are
/// structured prefix/flags/path matches and never include VCS mutation,
/// package install, package scripts, publish, network, interpreters, or shell
/// commands.
pub(crate) const PROFILES: &[CuratedProfile] = &[
    CuratedProfile {
        id: SAFE_READ_PROFILE,
        entries: &[
            // File and directory inspection.
            ProfileEntry {
                executable: "ls",
                argv: &[],
                args: ProfileArgPolicy {
                    flags: LS_FLAGS,
                    value_flags: &[],
                    allow_pattern: false,
                    allow_paths: true,
                },
                timeout_cap: Duration::from_secs(30),
                output_cap: 1024 * 1024,
            },
            ProfileEntry {
                executable: "pwd",
                argv: &[],
                args: ProfileArgPolicy::EXACT,
                timeout_cap: Duration::from_secs(30),
                output_cap: 64 * 1024,
            },
            ProfileEntry {
                executable: "cat",
                argv: &[],
                args: ProfileArgPolicy {
                    flags: &[],
                    value_flags: &[],
                    allow_pattern: false,
                    allow_paths: true,
                },
                timeout_cap: Duration::from_secs(30),
                output_cap: 1024 * 1024,
            },
            ProfileEntry {
                executable: "head",
                argv: &[],
                args: ProfileArgPolicy {
                    flags: HEAD_FLAGS,
                    value_flags: HEAD_VALUE_FLAGS,
                    allow_pattern: false,
                    allow_paths: true,
                },
                timeout_cap: Duration::from_secs(30),
                output_cap: 1024 * 1024,
            },
            ProfileEntry {
                executable: "tail",
                argv: &[],
                args: ProfileArgPolicy {
                    flags: HEAD_FLAGS,
                    value_flags: HEAD_VALUE_FLAGS,
                    allow_pattern: false,
                    allow_paths: true,
                },
                timeout_cap: Duration::from_secs(30),
                output_cap: 1024 * 1024,
            },
            ProfileEntry {
                executable: "wc",
                argv: &[],
                args: ProfileArgPolicy {
                    flags: WC_FLAGS,
                    value_flags: &[],
                    allow_pattern: false,
                    allow_paths: true,
                },
                timeout_cap: Duration::from_secs(30),
                output_cap: 256 * 1024,
            },
            ProfileEntry {
                executable: "stat",
                argv: &[],
                args: ProfileArgPolicy {
                    flags: &["-L", "-f"],
                    value_flags: STAT_VALUE_FLAGS,
                    allow_pattern: false,
                    allow_paths: true,
                },
                timeout_cap: Duration::from_secs(30),
                output_cap: 256 * 1024,
            },
            ProfileEntry {
                executable: "file",
                argv: &[],
                args: ProfileArgPolicy {
                    flags: FILE_FLAGS,
                    value_flags: &[],
                    allow_pattern: false,
                    allow_paths: true,
                },
                timeout_cap: Duration::from_secs(30),
                output_cap: 256 * 1024,
            },
            ProfileEntry {
                executable: "du",
                argv: &[],
                args: ProfileArgPolicy {
                    flags: DU_FLAGS,
                    value_flags: DU_VALUE_FLAGS,
                    allow_pattern: false,
                    allow_paths: true,
                },
                timeout_cap: Duration::from_secs(60),
                output_cap: 1024 * 1024,
            },
            ProfileEntry {
                executable: "tree",
                argv: &[],
                args: ProfileArgPolicy {
                    flags: TREE_FLAGS,
                    value_flags: TREE_VALUE_FLAGS,
                    allow_pattern: false,
                    allow_paths: true,
                },
                timeout_cap: Duration::from_secs(30),
                output_cap: 1024 * 1024,
            },
            ProfileEntry {
                executable: "find",
                argv: &[],
                args: ProfileArgPolicy {
                    flags: FIND_FLAGS,
                    value_flags: FIND_VALUE_FLAGS,
                    allow_pattern: false,
                    allow_paths: true,
                },
                timeout_cap: Duration::from_secs(60),
                output_cap: 1024 * 1024,
            },
            ProfileEntry {
                executable: "fd",
                argv: &[],
                args: ProfileArgPolicy {
                    flags: FD_FLAGS,
                    value_flags: FD_VALUE_FLAGS,
                    allow_pattern: true,
                    allow_paths: true,
                },
                timeout_cap: Duration::from_secs(60),
                output_cap: 1024 * 1024,
            },
            ProfileEntry {
                executable: "basename",
                argv: &[],
                args: ProfileArgPolicy {
                    flags: &[],
                    value_flags: &[],
                    allow_pattern: false,
                    allow_paths: true,
                },
                timeout_cap: Duration::from_secs(30),
                output_cap: 64 * 1024,
            },
            ProfileEntry {
                executable: "dirname",
                argv: &[],
                args: ProfileArgPolicy {
                    flags: &[],
                    value_flags: &[],
                    allow_pattern: false,
                    allow_paths: true,
                },
                timeout_cap: Duration::from_secs(30),
                output_cap: 64 * 1024,
            },
            ProfileEntry {
                executable: "realpath",
                argv: &[],
                args: ProfileArgPolicy {
                    flags: &[],
                    value_flags: &[],
                    allow_pattern: false,
                    allow_paths: true,
                },
                timeout_cap: Duration::from_secs(30),
                output_cap: 64 * 1024,
            },
            ProfileEntry {
                executable: "readlink",
                argv: &[],
                args: ProfileArgPolicy {
                    flags: &[],
                    value_flags: &[],
                    allow_pattern: false,
                    allow_paths: true,
                },
                timeout_cap: Duration::from_secs(30),
                output_cap: 64 * 1024,
            },
            // Comparison and checksums.
            ProfileEntry {
                executable: "diff",
                argv: &[],
                args: ProfileArgPolicy {
                    flags: DIFF_FLAGS,
                    value_flags: &[],
                    allow_pattern: false,
                    allow_paths: true,
                },
                timeout_cap: Duration::from_secs(60),
                output_cap: 1024 * 1024,
            },
            ProfileEntry {
                executable: "cmp",
                argv: &[],
                args: ProfileArgPolicy {
                    flags: &["-s", "-l", "-b"],
                    value_flags: &[],
                    allow_pattern: false,
                    allow_paths: true,
                },
                timeout_cap: Duration::from_secs(60),
                output_cap: 256 * 1024,
            },
            ProfileEntry {
                executable: "sha256sum",
                argv: &[],
                args: ProfileArgPolicy {
                    flags: CHECKSUM_FLAGS,
                    value_flags: &[],
                    allow_pattern: false,
                    allow_paths: true,
                },
                timeout_cap: Duration::from_secs(60),
                output_cap: 256 * 1024,
            },
            ProfileEntry {
                executable: "sha1sum",
                argv: &[],
                args: ProfileArgPolicy {
                    flags: CHECKSUM_FLAGS,
                    value_flags: &[],
                    allow_pattern: false,
                    allow_paths: true,
                },
                timeout_cap: Duration::from_secs(60),
                output_cap: 256 * 1024,
            },
            ProfileEntry {
                executable: "md5sum",
                argv: &[],
                args: ProfileArgPolicy {
                    flags: CHECKSUM_FLAGS,
                    value_flags: &[],
                    allow_pattern: false,
                    allow_paths: true,
                },
                timeout_cap: Duration::from_secs(60),
                output_cap: 256 * 1024,
            },
            ProfileEntry {
                executable: "cksum",
                argv: &[],
                args: ProfileArgPolicy {
                    flags: READ_FLAGS,
                    value_flags: &[],
                    allow_pattern: false,
                    allow_paths: true,
                },
                timeout_cap: Duration::from_secs(60),
                output_cap: 256 * 1024,
            },
            // Text filtering (no interpreters, no in-place edits).
            ProfileEntry {
                executable: "sort",
                argv: &[],
                args: ProfileArgPolicy {
                    flags: SORT_FLAGS,
                    value_flags: SORT_VALUE_FLAGS,
                    allow_pattern: false,
                    allow_paths: true,
                },
                timeout_cap: Duration::from_secs(60),
                output_cap: 1024 * 1024,
            },
            ProfileEntry {
                executable: "uniq",
                argv: &[],
                args: ProfileArgPolicy {
                    flags: &["-c", "-d", "-u", "-i"],
                    value_flags: &[],
                    allow_pattern: false,
                    allow_paths: true,
                },
                timeout_cap: Duration::from_secs(60),
                output_cap: 1024 * 1024,
            },
            ProfileEntry {
                executable: "cut",
                argv: &[],
                args: ProfileArgPolicy {
                    flags: CUT_FLAGS,
                    value_flags: CUT_VALUE_FLAGS,
                    allow_pattern: false,
                    allow_paths: true,
                },
                timeout_cap: Duration::from_secs(60),
                output_cap: 1024 * 1024,
            },
            ProfileEntry {
                executable: "comm",
                argv: &[],
                args: ProfileArgPolicy {
                    flags: COMM_FLAGS,
                    value_flags: &[],
                    allow_pattern: false,
                    allow_paths: true,
                },
                timeout_cap: Duration::from_secs(60),
                output_cap: 1024 * 1024,
            },
            ProfileEntry {
                executable: "grep",
                argv: &[],
                args: ProfileArgPolicy {
                    flags: GREP_FLAGS,
                    value_flags: GREP_VALUE_FLAGS,
                    allow_pattern: true,
                    allow_paths: true,
                },
                timeout_cap: Duration::from_secs(60),
                output_cap: 1024 * 1024,
            },
            ProfileEntry {
                executable: "egrep",
                argv: &[],
                args: ProfileArgPolicy {
                    flags: GREP_FLAGS,
                    value_flags: GREP_VALUE_FLAGS,
                    allow_pattern: true,
                    allow_paths: true,
                },
                timeout_cap: Duration::from_secs(60),
                output_cap: 1024 * 1024,
            },
            ProfileEntry {
                executable: "fgrep",
                argv: &[],
                args: ProfileArgPolicy {
                    flags: GREP_FLAGS,
                    value_flags: GREP_VALUE_FLAGS,
                    allow_pattern: true,
                    allow_paths: true,
                },
                timeout_cap: Duration::from_secs(60),
                output_cap: 1024 * 1024,
            },
            ProfileEntry {
                executable: "rg",
                argv: &[],
                args: ProfileArgPolicy {
                    flags: RG_FLAGS,
                    value_flags: RG_VALUE_FLAGS,
                    allow_pattern: true,
                    allow_paths: true,
                },
                timeout_cap: Duration::from_secs(60),
                output_cap: 1024 * 1024,
            },
            // Git read-only inspection.
            ProfileEntry {
                executable: "git",
                argv: &["status"],
                args: ProfileArgPolicy {
                    flags: &["-s", "--short", "-b", "--branch", "--porcelain", "--porcelain=v1"],
                    value_flags: &[],
                    allow_pattern: false,
                    allow_paths: true,
                },
                timeout_cap: Duration::from_secs(60),
                output_cap: 1024 * 1024,
            },
            ProfileEntry {
                executable: "git",
                argv: &["diff"],
                args: ProfileArgPolicy {
                    flags: GIT_DIFF_FLAGS,
                    value_flags: GIT_DIFF_VALUE_FLAGS,
                    allow_pattern: false,
                    allow_paths: true,
                },
                timeout_cap: Duration::from_secs(60),
                output_cap: 1024 * 1024,
            },
            ProfileEntry {
                executable: "git",
                argv: &["log"],
                args: ProfileArgPolicy {
                    flags: GIT_LOG_FLAGS,
                    value_flags: GIT_LOG_VALUE_FLAGS,
                    allow_pattern: false,
                    allow_paths: true,
                },
                timeout_cap: Duration::from_secs(60),
                output_cap: 1024 * 1024,
            },
            ProfileEntry {
                executable: "git",
                argv: &["show"],
                args: ProfileArgPolicy {
                    flags: GIT_SHOW_FLAGS,
                    value_flags: GIT_SHOW_VALUE_FLAGS,
                    allow_pattern: false,
                    allow_paths: true,
                },
                timeout_cap: Duration::from_secs(60),
                output_cap: 1024 * 1024,
            },
            ProfileEntry {
                executable: "git",
                argv: &["blame"],
                args: ProfileArgPolicy {
                    flags: GIT_BLAME_FLAGS,
                    value_flags: GIT_BLAME_VALUE_FLAGS,
                    allow_pattern: false,
                    allow_paths: true,
                },
                timeout_cap: Duration::from_secs(60),
                output_cap: 1024 * 1024,
            },
            ProfileEntry {
                executable: "git",
                argv: &["ls-files"],
                args: ProfileArgPolicy {
                    flags: GIT_LS_FILES_FLAGS,
                    value_flags: &[],
                    allow_pattern: false,
                    allow_paths: true,
                },
                timeout_cap: Duration::from_secs(60),
                output_cap: 1024 * 1024,
            },
            ProfileEntry {
                executable: "git",
                argv: &["ls-tree"],
                args: ProfileArgPolicy {
                    flags: GIT_LS_TREE_FLAGS,
                    value_flags: &[],
                    allow_pattern: false,
                    allow_paths: true,
                },
                timeout_cap: Duration::from_secs(60),
                output_cap: 1024 * 1024,
            },
            ProfileEntry {
                executable: "git",
                argv: &["rev-parse"],
                args: ProfileArgPolicy {
                    flags: GIT_REV_PARSE_FLAGS,
                    value_flags: &[],
                    allow_pattern: false,
                    allow_paths: true,
                },
                timeout_cap: Duration::from_secs(60),
                output_cap: 256 * 1024,
            },
            ProfileEntry {
                executable: "git",
                argv: &["describe"],
                args: ProfileArgPolicy {
                    flags: GIT_DESCRIBE_FLAGS,
                    value_flags: &[],
                    allow_pattern: false,
                    allow_paths: true,
                },
                timeout_cap: Duration::from_secs(60),
                output_cap: 256 * 1024,
            },
            ProfileEntry {
                executable: "git",
                argv: &["shortlog"],
                args: ProfileArgPolicy {
                    flags: GIT_SHORTLOG_FLAGS,
                    value_flags: GIT_SHORTLOG_VALUE_FLAGS,
                    allow_pattern: false,
                    allow_paths: true,
                },
                timeout_cap: Duration::from_secs(60),
                output_cap: 1024 * 1024,
            },
            ProfileEntry {
                executable: "git",
                argv: &["name-rev"],
                args: ProfileArgPolicy {
                    flags: GIT_NAME_REV_FLAGS,
                    value_flags: &[],
                    allow_pattern: false,
                    allow_paths: true,
                },
                timeout_cap: Duration::from_secs(60),
                output_cap: 1024 * 1024,
            },
            ProfileEntry {
                executable: "git",
                argv: &["grep"],
                args: ProfileArgPolicy {
                    flags: GIT_GREP_FLAGS,
                    value_flags: GIT_GREP_VALUE_FLAGS,
                    allow_pattern: true,
                    allow_paths: true,
                },
                timeout_cap: Duration::from_secs(60),
                output_cap: 1024 * 1024,
            },
            ProfileEntry {
                executable: "git",
                argv: &["check-ignore"],
                args: ProfileArgPolicy {
                    flags: GIT_CHECK_IGNORE_FLAGS,
                    value_flags: &[],
                    allow_pattern: false,
                    allow_paths: true,
                },
                timeout_cap: Duration::from_secs(30),
                output_cap: 256 * 1024,
            },
            ProfileEntry {
                executable: "git",
                argv: &["count-objects"],
                args: ProfileArgPolicy {
                    flags: &["-v", "-H"],
                    value_flags: &[],
                    allow_pattern: false,
                    allow_paths: false,
                },
                timeout_cap: Duration::from_secs(60),
                output_cap: 256 * 1024,
            },
            ProfileEntry {
                executable: "git",
                argv: &["branch", "--show-current"],
                args: ProfileArgPolicy::EXACT,
                timeout_cap: Duration::from_secs(60),
                output_cap: 1024 * 1024,
            },
            ProfileEntry {
                executable: "git",
                argv: &["branch", "--list"],
                args: ProfileArgPolicy {
                    flags: GIT_BRANCH_FLAGS,
                    value_flags: &[],
                    allow_pattern: false,
                    allow_paths: true,
                },
                timeout_cap: Duration::from_secs(60),
                output_cap: 1024 * 1024,
            },
            ProfileEntry {
                executable: "git",
                argv: &["branch", "-l"],
                args: ProfileArgPolicy {
                    flags: GIT_BRANCH_FLAGS,
                    value_flags: &[],
                    allow_pattern: false,
                    allow_paths: true,
                },
                timeout_cap: Duration::from_secs(60),
                output_cap: 1024 * 1024,
            },
            ProfileEntry {
                executable: "git",
                argv: &["tag", "-l"],
                args: ProfileArgPolicy {
                    flags: GIT_TAG_FLAGS,
                    value_flags: GIT_TAG_VALUE_FLAGS,
                    allow_pattern: false,
                    allow_paths: true,
                },
                timeout_cap: Duration::from_secs(60),
                output_cap: 1024 * 1024,
            },
            ProfileEntry {
                executable: "git",
                argv: &["stash", "list"],
                args: ProfileArgPolicy::EXACT,
                timeout_cap: Duration::from_secs(60),
                output_cap: 1024 * 1024,
            },
            ProfileEntry {
                executable: "git",
                argv: &["worktree", "list"],
                args: ProfileArgPolicy {
                    flags: &["--porcelain"],
                    value_flags: &[],
                    allow_pattern: false,
                    allow_paths: false,
                },
                timeout_cap: Duration::from_secs(60),
                output_cap: 256 * 1024,
            },
            // Local package inventory (never install/update/publish).
            ProfileEntry {
                executable: "npm",
                argv: &["list"],
                args: ProfileArgPolicy {
                    flags: NPM_LIST_FLAGS,
                    value_flags: NPM_LIST_VALUE_FLAGS,
                    allow_pattern: false,
                    allow_paths: false,
                },
                timeout_cap: Duration::from_secs(120),
                output_cap: 1024 * 1024,
            },
            ProfileEntry {
                executable: "npm",
                argv: &["ls"],
                args: ProfileArgPolicy {
                    flags: NPM_LIST_FLAGS,
                    value_flags: NPM_LIST_VALUE_FLAGS,
                    allow_pattern: false,
                    allow_paths: false,
                },
                timeout_cap: Duration::from_secs(120),
                output_cap: 1024 * 1024,
            },
            ProfileEntry {
                executable: "yarn",
                argv: &["list"],
                args: ProfileArgPolicy {
                    flags: &["--depth", "--json", "--pattern", "--silent"],
                    value_flags: &["--depth", "--pattern"],
                    allow_pattern: false,
                    allow_paths: false,
                },
                timeout_cap: Duration::from_secs(120),
                output_cap: 1024 * 1024,
            },
            ProfileEntry {
                executable: "pnpm",
                argv: &["list"],
                args: ProfileArgPolicy {
                    flags: &["--depth", "--json", "--prod", "--dev", "--silent"],
                    value_flags: &["--depth"],
                    allow_pattern: false,
                    allow_paths: false,
                },
                timeout_cap: Duration::from_secs(120),
                output_cap: 1024 * 1024,
            },
            ProfileEntry {
                executable: "node",
                argv: &["--version"],
                args: ProfileArgPolicy::EXACT,
                timeout_cap: Duration::from_secs(30),
                output_cap: 64 * 1024,
            },
            // System information.
            ProfileEntry {
                executable: "echo",
                argv: &[],
                args: ProfileArgPolicy {
                    flags: &["-n", "-e", "-E"],
                    value_flags: &[],
                    allow_pattern: true,
                    allow_paths: false,
                },
                timeout_cap: Duration::from_secs(30),
                output_cap: 64 * 1024,
            },
            ProfileEntry {
                executable: "printf",
                argv: &[],
                args: ProfileArgPolicy {
                    flags: &[],
                    value_flags: &[],
                    allow_pattern: true,
                    allow_paths: false,
                },
                timeout_cap: Duration::from_secs(30),
                output_cap: 64 * 1024,
            },
            ProfileEntry {
                executable: "date",
                argv: &[],
                args: ProfileArgPolicy {
                    flags: &["-u", "--utc", "-R", "--rfc-3339"],
                    value_flags: &[],
                    allow_pattern: false,
                    allow_paths: false,
                },
                timeout_cap: Duration::from_secs(30),
                output_cap: 64 * 1024,
            },
            ProfileEntry {
                executable: "uname",
                argv: &[],
                args: ProfileArgPolicy {
                    flags: &["-a", "-s", "-r", "-m"],
                    value_flags: &[],
                    allow_pattern: false,
                    allow_paths: false,
                },
                timeout_cap: Duration::from_secs(30),
                output_cap: 64 * 1024,
            },
            ProfileEntry {
                executable: "whoami",
                argv: &[],
                args: ProfileArgPolicy::EXACT,
                timeout_cap: Duration::from_secs(30),
                output_cap: 64 * 1024,
            },
            ProfileEntry {
                executable: "id",
                argv: &[],
                args: ProfileArgPolicy::EXACT,
                timeout_cap: Duration::from_secs(30),
                output_cap: 64 * 1024,
            },
            ProfileEntry {
                executable: "hostname",
                argv: &[],
                args: ProfileArgPolicy::EXACT,
                timeout_cap: Duration::from_secs(30),
                output_cap: 64 * 1024,
            },
            ProfileEntry {
                executable: "nproc",
                argv: &[],
                args: ProfileArgPolicy::EXACT,
                timeout_cap: Duration::from_secs(30),
                output_cap: 64 * 1024,
            },
            ProfileEntry {
                executable: "groups",
                argv: &[],
                args: ProfileArgPolicy::EXACT,
                timeout_cap: Duration::from_secs(30),
                output_cap: 64 * 1024,
            },
            ProfileEntry {
                executable: "which",
                argv: &[],
                args: ProfileArgPolicy {
                    flags: &["-a"],
                    value_flags: &[],
                    allow_pattern: true,
                    allow_paths: false,
                },
                timeout_cap: Duration::from_secs(30),
                output_cap: 64 * 1024,
            },
            ProfileEntry {
                executable: "ps",
                argv: &[],
                args: ProfileArgPolicy {
                    flags: PS_FLAGS,
                    value_flags: &[],
                    allow_pattern: true,
                    allow_paths: false,
                },
                timeout_cap: Duration::from_secs(30),
                output_cap: 512 * 1024,
            },
            ProfileEntry {
                executable: "df",
                argv: &[],
                args: ProfileArgPolicy {
                    flags: DF_FLAGS,
                    value_flags: &[],
                    allow_pattern: false,
                    allow_paths: true,
                },
                timeout_cap: Duration::from_secs(30),
                output_cap: 256 * 1024,
            },
            ProfileEntry {
                executable: "free",
                argv: &[],
                args: ProfileArgPolicy {
                    flags: &["-h", "-m", "-g", "-b"],
                    value_flags: &[],
                    allow_pattern: false,
                    allow_paths: false,
                },
                timeout_cap: Duration::from_secs(30),
                output_cap: 64 * 1024,
            },
            ProfileEntry {
                executable: "uptime",
                argv: &[],
                args: ProfileArgPolicy::EXACT,
                timeout_cap: Duration::from_secs(30),
                output_cap: 64 * 1024,
            },
            ProfileEntry {
                executable: "locale",
                argv: &[],
                args: ProfileArgPolicy {
                    flags: &["-a", "-m"],
                    value_flags: &[],
                    allow_pattern: false,
                    allow_paths: false,
                },
                timeout_cap: Duration::from_secs(30),
                output_cap: 256 * 1024,
            },
        ],
    },
    CuratedProfile {
        id: "git_readonly",
        entries: &[
            ProfileEntry {
                executable: "git",
                argv: &["status"],
                args: ProfileArgPolicy::EXACT,
                timeout_cap: Duration::from_secs(60),
                output_cap: 1024 * 1024,
            },
            ProfileEntry {
                executable: "git",
                argv: &["diff"],
                args: ProfileArgPolicy::EXACT,
                timeout_cap: Duration::from_secs(60),
                output_cap: 1024 * 1024,
            },
            ProfileEntry {
                executable: "git",
                argv: &["log"],
                args: ProfileArgPolicy::EXACT,
                timeout_cap: Duration::from_secs(60),
                output_cap: 1024 * 1024,
            },
            ProfileEntry {
                executable: "git",
                argv: &["show"],
                args: ProfileArgPolicy::EXACT,
                timeout_cap: Duration::from_secs(60),
                output_cap: 1024 * 1024,
            },
            ProfileEntry {
                executable: "git",
                argv: &["branch", "--show-current"],
                args: ProfileArgPolicy::EXACT,
                timeout_cap: Duration::from_secs(60),
                output_cap: 1024 * 1024,
            },
        ],
    },
    CuratedProfile {
        id: TERMINAL_READONLY_PROFILE,
        entries: &[
            ProfileEntry {
                executable: "pwd",
                argv: &[],
                args: ProfileArgPolicy::EXACT,
                timeout_cap: Duration::from_secs(30),
                output_cap: 64 * 1024,
            },
            ProfileEntry {
                executable: "ls",
                argv: &[],
                args: ProfileArgPolicy::EXACT,
                timeout_cap: Duration::from_secs(30),
                output_cap: 64 * 1024,
            },
            ProfileEntry {
                executable: "ls",
                argv: &["-a"],
                args: ProfileArgPolicy::EXACT,
                timeout_cap: Duration::from_secs(30),
                output_cap: 64 * 1024,
            },
            ProfileEntry {
                executable: "ls",
                argv: &["-l"],
                args: ProfileArgPolicy::EXACT,
                timeout_cap: Duration::from_secs(30),
                output_cap: 64 * 1024,
            },
            ProfileEntry {
                executable: "ls",
                argv: &["-la"],
                args: ProfileArgPolicy::EXACT,
                timeout_cap: Duration::from_secs(30),
                output_cap: 64 * 1024,
            },
            ProfileEntry {
                executable: "ls",
                argv: &["-al"],
                args: ProfileArgPolicy::EXACT,
                timeout_cap: Duration::from_secs(30),
                output_cap: 64 * 1024,
            },
        ],
    },
    CuratedProfile {
        id: "rust_validate",
        entries: &[
            ProfileEntry {
                executable: "cargo",
                argv: &["fmt", "--check"],
                args: ProfileArgPolicy::EXACT,
                timeout_cap: Duration::from_secs(300),
                output_cap: 1024 * 1024,
            },
            ProfileEntry {
                executable: "cargo",
                argv: &["test", "--quiet"],
                args: ProfileArgPolicy::EXACT,
                timeout_cap: Duration::from_secs(300),
                output_cap: 1024 * 1024,
            },
            ProfileEntry {
                executable: "cargo",
                argv: &["clippy"],
                args: ProfileArgPolicy::EXACT,
                timeout_cap: Duration::from_secs(300),
                output_cap: 1024 * 1024,
            },
        ],
    },
];

/// Exact structured lookup of one command in the curated registry; returns
/// the profile id and the matched entry. Only entries whose full argv equals
/// the entry argv match here. Grant-required profiles are searched first so
/// persisted profile grants keep their exact semantics.
pub(crate) fn match_profile_entry(
    executable: &str,
    argv: &[String],
) -> Option<(&'static str, &'static ProfileEntry)> {
    for profile in ordered_profiles() {
        for entry in profile.entries {
            if entry.executable == executable && entry.argv == argv {
                return Some((profile.id, entry));
            }
        }
    }
    None
}

/// Registry match order: grant-required profiles first (exact grants keep
/// winning), the built-in read-only allowlist last.
const MATCH_ORDER: [&str; 4] =
    ["git_readonly", TERMINAL_READONLY_PROFILE, "rust_validate", SAFE_READ_PROFILE];

fn ordered_profiles() -> impl Iterator<Item = &'static CuratedProfile> {
    MATCH_ORDER.iter().filter_map(|id| PROFILES.iter().find(|profile| profile.id == *id))
}

/// Structured lookup including trailing-argument policies. Returns every
/// matching registry entry in registry order, each with the path operands the
/// approval layer must validate against the canonical workspace. Executables
/// are direct argv (never shell text); flag allowlists reject every unknown
/// `-` token.
pub(crate) fn match_profile_candidates<'a>(
    executable: &str,
    argv: &'a [String],
) -> Vec<ProfileMatch<'a>> {
    let mut matches = Vec::new();
    for profile in ordered_profiles() {
        for entry in profile.entries {
            if entry.executable != executable || !argv_has_prefix(argv, entry.argv) {
                continue;
            }
            let trailing = &argv[entry.argv.len()..];
            if trailing.is_empty() {
                matches.push(ProfileMatch {
                    profile: profile.id,
                    entry,
                    path_operands: Vec::new(),
                });
                continue;
            }
            if let Some(path_operands) = match_trailing(entry.args, trailing) {
                matches.push(ProfileMatch { profile: profile.id, entry, path_operands });
            }
        }
    }
    matches
}

fn argv_has_prefix(argv: &[String], prefix: &[&str]) -> bool {
    argv.len() >= prefix.len()
        && prefix.iter().zip(argv.iter()).all(|(expected, actual)| actual == expected)
}

fn match_trailing(policy: ProfileArgPolicy, trailing: &[String]) -> Option<Vec<&str>> {
    if policy.flags.is_empty()
        && policy.value_flags.is_empty()
        && !policy.allow_pattern
        && !policy.allow_paths
    {
        return None;
    }
    let mut operands = Vec::new();
    let mut pattern_used = false;
    let mut index = 0;
    while index < trailing.len() {
        let token = trailing[index].as_str();
        if token.is_empty() || token.chars().any(char::is_control) {
            return None;
        }
        if token.starts_with('-') {
            if policy.value_flags.contains(&token) {
                if index + 1 >= trailing.len() {
                    return None;
                }
                index += 2;
                continue;
            }
            if policy.flags.contains(&token) {
                index += 1;
                continue;
            }
            // Long `--flag=value` tokens are accepted when the flag part is an
            // allowed flag or value flag; the value stays opaque. This keeps
            // `--output=…`-style writes out (the flag part is never allowed).
            if let Some(long) = token.strip_prefix("--")
                && let Some((name, _value)) = long.split_once('=')
            {
                let flag = format!("--{name}");
                if policy.flags.contains(&flag.as_str())
                    || policy.value_flags.contains(&flag.as_str())
                {
                    index += 1;
                    continue;
                }
                return None;
            }
            // Combined short flags (`-rn`, `-sh`, `-la`) are accepted when
            // every letter is individually allowlisted; long flags always
            // require an exact entry.
            if token.len() > 2
                && !token.starts_with("--")
                && let Some(letters) = token.strip_prefix('-')
                && letters.chars().all(|letter| {
                    let single = format!("-{letter}");
                    policy.flags.contains(&single.as_str())
                })
            {
                index += 1;
                continue;
            }
            return None;
        }
        if policy.allow_pattern && !pattern_used {
            pattern_used = true;
            index += 1;
            continue;
        }
        if policy.allow_paths {
            operands.push(token);
            index += 1;
            continue;
        }
        // Pattern-only entries accept every remaining non-flag token as free
        // text (`echo hello world`, `ps aux`).
        if policy.allow_pattern {
            index += 1;
            continue;
        }
        return None;
    }
    Some(operands)
}

/// Whether `id` is a known curated profile; unknown ids are rejected.
pub(crate) fn is_known_profile(id: &str) -> bool {
    PROFILES.iter().any(|profile| profile.id == id)
}
