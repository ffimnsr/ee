//! Editor configuration loading for ee.
//!
//! Settings are resolved by merging layers in priority order (lowest first):
//!   1. built-in defaults
//!   2. `/etc/ee/config.toml`
//!   3. `$XDG_CONFIG_HOME/ee/config.toml` or `~/.config/ee/config.toml`
//!   4. fallback `~/.ee.toml` when XDG user config is missing
//!   5. every ancestor `.ee.toml` from outermost to innermost
//!   6. `.editorconfig` (walked up from the open file, per spec)
//!
//! Later layers override earlier ones for any key that is explicitly set.

#[cfg(any(feature = "agents", test))]
use super::init::validate_config_contents;
use super::raw::WorkspaceMemoryToml;
#[cfg(any(feature = "agents", test))]
use std::fs;
#[cfg(any(feature = "agents", test))]
use std::io::{ErrorKind, Write as _};
#[cfg(any(feature = "agents", test))]
use std::path::Path;

#[cfg(any(feature = "agents", test))]
use ee_agent_host::{
    DEFAULT_WORKSPACE_MEMORY_CANDIDATE_RETENTION_DAYS, DEFAULT_WORKSPACE_MEMORY_EXPIRY_DAYS,
    DEFAULT_WORKSPACE_MEMORY_STALE_RETENTION_DAYS,
    DEFAULT_WORKSPACE_MEMORY_SUPERSEDED_RETENTION_DAYS, WorkspaceMemoryHostConfig,
};

const MAX_WORKSPACE_MEMORY_VALUE_BYTES: usize = 1024 * 1024;
pub(super) const MAX_WORKSPACE_MEMORY_ACTIVE_FACTS: usize = 100_000;
const MAX_WORKSPACE_MEMORY_ACTIVE_BYTES: usize = 1024 * 1024 * 1024;
const MAX_WORKSPACE_MEMORY_TOTAL_FACTS: usize = 100_000;
const MAX_WORKSPACE_MEMORY_TOTAL_BYTES: usize = 1024 * 1024 * 1024;
pub(super) const MAX_WORKSPACE_MEMORY_RECALL_RESULTS: usize = 100;
const MAX_WORKSPACE_MEMORY_BUSY_TIMEOUT_MS: u64 = 60_000;
pub(super) const MAX_WORKSPACE_MEMORY_RETENTION_DAYS: u64 = 3_650;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct WorkspaceMemorySettings {
    pub enabled: bool,
    pub max_value_bytes: usize,
    pub max_active_facts: usize,
    pub max_active_bytes: usize,
    pub max_total_facts: usize,
    pub max_total_bytes: usize,
    pub max_recall_results: usize,
    pub busy_timeout_ms: u64,
    pub default_expiry_days: u64,
    pub candidate_retention_days: u64,
    pub stale_retention_days: u64,
    pub superseded_retention_days: u64,
}

impl Default for WorkspaceMemorySettings {
    fn default() -> Self {
        #[cfg(any(feature = "agents", test))]
        {
            let defaults = WorkspaceMemoryHostConfig::default();
            Self {
                enabled: false,
                max_value_bytes: defaults.quotas.max_value_bytes,
                max_active_facts: defaults.quotas.max_active_facts,
                max_active_bytes: defaults.quotas.max_active_bytes,
                max_total_facts: defaults.quotas.max_total_facts,
                max_total_bytes: defaults.quotas.max_total_bytes,
                max_recall_results: defaults.quotas.max_recall_results,
                busy_timeout_ms: u64::try_from(defaults.busy_timeout.as_millis())
                    .unwrap_or(MAX_WORKSPACE_MEMORY_BUSY_TIMEOUT_MS),
                default_expiry_days: DEFAULT_WORKSPACE_MEMORY_EXPIRY_DAYS,
                candidate_retention_days: DEFAULT_WORKSPACE_MEMORY_CANDIDATE_RETENTION_DAYS,
                stale_retention_days: DEFAULT_WORKSPACE_MEMORY_STALE_RETENTION_DAYS,
                superseded_retention_days: DEFAULT_WORKSPACE_MEMORY_SUPERSEDED_RETENTION_DAYS,
            }
        }
        #[cfg(not(any(feature = "agents", test)))]
        {
            Self {
                enabled: false,
                max_value_bytes: 4 * 1024,
                max_active_facts: 256,
                max_active_bytes: 512 * 1024,
                max_total_facts: 256,
                max_total_bytes: 512 * 1024,
                max_recall_results: 8,
                busy_timeout_ms: 2_000,
                default_expiry_days: 0,
                candidate_retention_days: 7,
                stale_retention_days: 30,
                superseded_retention_days: 90,
            }
        }
    }
}

pub(super) fn merge_workspace_memory(
    resolved: &mut WorkspaceMemorySettings,
    patch: &WorkspaceMemoryToml,
) {
    if let Some(enabled) = patch.enabled {
        resolved.enabled = enabled;
    }
    merge_bounded_usize(
        "agents.workspace_memory.max_value_bytes",
        &mut resolved.max_value_bytes,
        patch.max_value_bytes,
        MAX_WORKSPACE_MEMORY_VALUE_BYTES,
    );
    merge_bounded_usize(
        "agents.workspace_memory.max_active_facts",
        &mut resolved.max_active_facts,
        patch.max_active_facts,
        MAX_WORKSPACE_MEMORY_ACTIVE_FACTS,
    );
    merge_bounded_usize(
        "agents.workspace_memory.max_active_bytes",
        &mut resolved.max_active_bytes,
        patch.max_active_bytes,
        MAX_WORKSPACE_MEMORY_ACTIVE_BYTES,
    );
    merge_bounded_usize(
        "agents.workspace_memory.max_total_facts",
        &mut resolved.max_total_facts,
        patch.max_total_facts,
        MAX_WORKSPACE_MEMORY_TOTAL_FACTS,
    );
    merge_bounded_usize(
        "agents.workspace_memory.max_total_bytes",
        &mut resolved.max_total_bytes,
        patch.max_total_bytes,
        MAX_WORKSPACE_MEMORY_TOTAL_BYTES,
    );
    merge_bounded_usize(
        "agents.workspace_memory.max_recall_results",
        &mut resolved.max_recall_results,
        patch.max_recall_results,
        MAX_WORKSPACE_MEMORY_RECALL_RESULTS,
    );
    merge_retention_days(
        "agents.workspace_memory.default_expiry_days",
        &mut resolved.default_expiry_days,
        patch.default_expiry_days,
        true,
    );
    merge_retention_days(
        "agents.workspace_memory.candidate_retention_days",
        &mut resolved.candidate_retention_days,
        patch.candidate_retention_days,
        false,
    );
    merge_retention_days(
        "agents.workspace_memory.stale_retention_days",
        &mut resolved.stale_retention_days,
        patch.stale_retention_days,
        false,
    );
    merge_retention_days(
        "agents.workspace_memory.superseded_retention_days",
        &mut resolved.superseded_retention_days,
        patch.superseded_retention_days,
        false,
    );
    if let Some(value) = patch.busy_timeout_ms {
        if (1..=MAX_WORKSPACE_MEMORY_BUSY_TIMEOUT_MS).contains(&value) {
            resolved.busy_timeout_ms = value;
        } else {
            eprintln!(
                "ee: warning: agents.workspace_memory.busy_timeout_ms must be between 1 and {MAX_WORKSPACE_MEMORY_BUSY_TIMEOUT_MS}; keeping {}",
                resolved.busy_timeout_ms
            );
        }
    }
}

fn merge_retention_days(name: &str, resolved: &mut u64, patch: Option<u64>, allow_zero: bool) {
    let Some(value) = patch else { return };
    let minimum = u64::from(!allow_zero);
    if (minimum..=MAX_WORKSPACE_MEMORY_RETENTION_DAYS).contains(&value) {
        *resolved = value;
    } else {
        eprintln!(
            "ee: warning: {name} must be between {minimum} and {MAX_WORKSPACE_MEMORY_RETENTION_DAYS}; keeping {resolved}"
        );
    }
}

fn merge_bounded_usize(name: &str, resolved: &mut usize, patch: Option<usize>, max: usize) {
    let Some(value) = patch else { return };
    if (1..=max).contains(&value) {
        *resolved = value;
    } else {
        eprintln!("ee: warning: {name} must be between 1 and {max}; keeping {resolved}");
    }
}

/// Rejects invalid agents/MCP server definitions and ids that collide across
/// the `agents.servers` and `mcp.servers` namespaces.
pub(super) fn validate_workspace_memory_toml(memory: &WorkspaceMemoryToml) -> Result<(), String> {
    for (name, value, max) in [
        ("max_value_bytes", memory.max_value_bytes, MAX_WORKSPACE_MEMORY_VALUE_BYTES),
        ("max_active_facts", memory.max_active_facts, MAX_WORKSPACE_MEMORY_ACTIVE_FACTS),
        ("max_active_bytes", memory.max_active_bytes, MAX_WORKSPACE_MEMORY_ACTIVE_BYTES),
        ("max_total_facts", memory.max_total_facts, MAX_WORKSPACE_MEMORY_TOTAL_FACTS),
        ("max_total_bytes", memory.max_total_bytes, MAX_WORKSPACE_MEMORY_TOTAL_BYTES),
        ("max_recall_results", memory.max_recall_results, MAX_WORKSPACE_MEMORY_RECALL_RESULTS),
    ] {
        if value.is_some_and(|value| !(1..=max).contains(&value)) {
            return Err(format!("agents.workspace_memory.{name} must be between 1 and {max}"));
        }
    }
    for (name, value, allow_zero) in [
        ("default_expiry_days", memory.default_expiry_days, true),
        ("candidate_retention_days", memory.candidate_retention_days, false),
        ("stale_retention_days", memory.stale_retention_days, false),
        ("superseded_retention_days", memory.superseded_retention_days, false),
    ] {
        let minimum = u64::from(!allow_zero);
        if value
            .is_some_and(|value| !(minimum..=MAX_WORKSPACE_MEMORY_RETENTION_DAYS).contains(&value))
        {
            return Err(format!(
                "agents.workspace_memory.{name} must be between {minimum} and {MAX_WORKSPACE_MEMORY_RETENTION_DAYS}"
            ));
        }
    }
    if memory
        .busy_timeout_ms
        .is_some_and(|value| !(1..=MAX_WORKSPACE_MEMORY_BUSY_TIMEOUT_MS).contains(&value))
    {
        return Err(format!(
            "agents.workspace_memory.busy_timeout_ms must be between 1 and {MAX_WORKSPACE_MEMORY_BUSY_TIMEOUT_MS}"
        ));
    }
    Ok(())
}

/// Persists an explicit workspace-memory switch without reserializing unrelated
/// config, comments, ordering, or formatting.
#[cfg(any(feature = "agents", test))]
pub(crate) fn persist_workspace_memory_enabled(path: &Path, enabled: bool) -> Result<(), String> {
    let existing = match fs::symlink_metadata(path) {
        Ok(metadata) => {
            if metadata.file_type().is_symlink() || !metadata.is_file() {
                return Err(format!(
                    "workspace config must be a regular non-symlink file: {}",
                    path.display()
                ));
            }
            Some(
                fs::read_to_string(path)
                    .map_err(|error| format!("cannot read {}: {error}", path.display()))?,
            )
        }
        Err(error) if error.kind() == ErrorKind::NotFound => None,
        Err(error) => return Err(format!("cannot inspect {}: {error}", path.display())),
    };
    let contents = existing.as_deref().unwrap_or_default();
    let parsed: toml::Value = toml::from_str(contents).map_err(|error| {
        format!("refusing to modify invalid config {}: {error}", path.display())
    })?;
    let has_memory_table =
        parsed.get("agents").and_then(|agents| agents.get("workspace_memory")).is_some();

    let newline = if contents.contains("\r\n") { "\r\n" } else { "\n" };
    let mut section_start = None;
    let mut section_end = contents.len();
    let mut line_start = 0;
    for line in contents.split_inclusive('\n') {
        let body = line.trim_end_matches(['\r', '\n']);
        let trimmed = body.trim();
        if trimmed == "[agents.workspace_memory]" {
            section_start = Some(line_start + line.len());
        } else if section_start.is_some() && trimmed.starts_with('[') {
            section_end = line_start;
            break;
        }
        line_start += line.len();
    }
    if section_start.is_none() && has_memory_table {
        return Err(format!(
            "refusing to rewrite non-canonical [agents.workspace_memory] table in {}",
            path.display()
        ));
    }

    let value = if enabled { "true" } else { "false" };
    let updated = if let Some(section_start) = section_start {
        let mut enabled_line = None;
        let mut offset = section_start;
        for line in contents[section_start..section_end].split_inclusive('\n') {
            let body = line.trim_end_matches(['\r', '\n']);
            let trimmed = body.trim_start();
            if trimmed
                .strip_prefix("enabled")
                .is_some_and(|rest| rest.trim_start().starts_with('='))
            {
                enabled_line = Some((offset, offset + body.len(), body));
                break;
            }
            offset += line.len();
        }
        if let Some((start, end, body)) = enabled_line {
            let indent_len = body.len() - body.trim_start().len();
            let comment = body.find('#').map(|index| &body[index..]).unwrap_or_default();
            let comment = if comment.is_empty() { String::new() } else { format!(" {comment}") };
            format!(
                "{}{}enabled = {value}{comment}{}",
                &contents[..start],
                &body[..indent_len],
                &contents[end..]
            )
        } else {
            let separator = if section_start == 0 || contents[..section_start].ends_with('\n') {
                ""
            } else {
                newline
            };
            format!(
                "{}{separator}enabled = {value}{newline}{}",
                &contents[..section_start],
                &contents[section_start..]
            )
        }
    } else {
        let separator = if contents.is_empty() || contents.ends_with('\n') { "" } else { newline };
        let blank = if contents.is_empty() { "" } else { newline };
        format!(
            "{contents}{separator}{blank}[agents.workspace_memory]{newline}enabled = {value}{newline}"
        )
    };

    validate_config_contents(path, &updated)?;
    write_config_atomically(path, updated.as_bytes())
}

#[cfg(any(feature = "agents", test))]
fn write_config_atomically(path: &Path, contents: &[u8]) -> Result<(), String> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent)
        .map_err(|error| format!("cannot create {}: {error}", parent.display()))?;
    let file_name = path.file_name().and_then(|name| name.to_str()).unwrap_or("config.toml");
    let mut temporary = None;
    for attempt in 0..100_u32 {
        let candidate = parent.join(format!(".{file_name}.{}.{}.tmp", std::process::id(), attempt));
        let mut options = fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt as _;
            options.mode(0o600);
        }
        match options.open(&candidate) {
            Ok(file) => {
                temporary = Some((candidate, file));
                break;
            }
            Err(error) if error.kind() == ErrorKind::AlreadyExists => {}
            Err(error) => {
                return Err(format!(
                    "cannot create temporary config in {}: {error}",
                    parent.display()
                ));
            }
        }
    }
    let (temporary_path, mut file) = temporary
        .ok_or_else(|| format!("cannot allocate temporary config in {}", parent.display()))?;
    let result = (|| {
        file.write_all(contents)
            .map_err(|error| format!("cannot write {}: {error}", temporary_path.display()))?;
        file.sync_all()
            .map_err(|error| format!("cannot sync {}: {error}", temporary_path.display()))?;
        if let Ok(metadata) = fs::metadata(path) {
            fs::set_permissions(&temporary_path, metadata.permissions()).map_err(|error| {
                format!("cannot preserve permissions for {}: {error}", path.display())
            })?;
        }
        fs::rename(&temporary_path, path)
            .map_err(|error| format!("cannot replace {}: {error}", path.display()))?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary_path);
    }
    result
}
