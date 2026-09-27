use std::collections::BTreeMap;
use std::io::{ErrorKind, Write as _};
use std::path::{Path, PathBuf};

use ee_agent_host::AgentError;

const INSTRUCTION_BYTES: usize = 16 * 1024;
const CONFIG_BYTES: usize = 8 * 1024;
const NOTE_BYTES: usize = 16 * 1024;
const NOTES_PER_SCOPE: usize = 50;
const NOTES_TOTAL_BYTES: usize = 128 * 1024;
const NOTES_READ_LIMIT: usize = 50;

/// Resolves the optional notes persistence path when `persist_notes` is
/// enabled. Returns `None` when the platform state directory is unavailable.
pub(crate) fn notes_persist_path(persist_notes: bool) -> Option<PathBuf> {
    if !persist_notes {
        return None;
    }
    match dirs::state_dir() {
        Some(state) => Some(state.join("ee").join("agent-notes.json")),
        None => {
            eprintln!(
                "ee: warning: agents.workspace_memory.persist_notes enabled but state directory is unavailable; notes stay in-memory"
            );
            None
        }
    }
}

#[derive(Debug, Default)]
pub(crate) struct ProjectKnowledge {
    notes: BTreeMap<String, BTreeMap<String, String>>,
    /// Optional durable notes file; `None` keeps notes in-memory only.
    persist_path: Option<PathBuf>,
}

impl ProjectKnowledge {
    /// Creates knowledge with optional durable note persistence. Existing
    /// persisted notes are loaded; a corrupt file is ignored and overwritten
    /// on the next save.
    pub(crate) fn new(persist_path: Option<PathBuf>) -> Self {
        let mut knowledge = Self { notes: BTreeMap::new(), persist_path };
        if let Some(path) = &knowledge.persist_path
            && let Ok(contents) = std::fs::read_to_string(path)
        {
            match serde_json::from_str::<BTreeMap<String, BTreeMap<String, String>>>(&contents) {
                Ok(notes) => knowledge.notes = notes,
                Err(error) => eprintln!(
                    "ee: warning: ignoring corrupt agent-notes file {}: {error}",
                    path.display()
                ),
            }
        }
        knowledge
    }

    /// Best-effort durable snapshot of all scoped notes. Failures never fail
    /// the caller; they degrade to in-memory behavior with a warning.
    fn persist_notes(&self) {
        let Some(path) = &self.persist_path else { return };
        let contents = match serde_json::to_vec(&self.notes) {
            Ok(contents) => contents,
            Err(error) => {
                eprintln!("ee: warning: cannot serialize agent notes: {error}");
                return;
            }
        };
        let parent = path.parent().unwrap_or_else(|| Path::new("."));
        if let Err(error) = std::fs::create_dir_all(parent) {
            eprintln!("ee: warning: cannot create {}: {error}", parent.display());
            return;
        }
        let mut temporary = None;
        for attempt in 0..100_u32 {
            let candidate =
                parent.join(format!(".agent-notes.{}.{}.tmp", std::process::id(), attempt));
            let mut options = std::fs::OpenOptions::new();
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
                    eprintln!("ee: warning: cannot create temporary notes file: {error}");
                    return;
                }
            }
        }
        let (temporary_path, mut file) = match temporary {
            Some(temporary) => temporary,
            None => {
                eprintln!("ee: warning: cannot allocate temporary notes file");
                return;
            }
        };
        let result = (|| -> Result<(), String> {
            file.write_all(&contents)
                .map_err(|error| format!("cannot write {}: {error}", temporary_path.display()))?;
            file.sync_all()
                .map_err(|error| format!("cannot sync {}: {error}", temporary_path.display()))?;
            std::fs::rename(&temporary_path, path)
                .map_err(|error| format!("cannot replace {}: {error}", path.display()))
        })();
        if let Err(error) = result {
            let _ = std::fs::remove_file(&temporary_path);
            eprintln!("ee: warning: agent notes persistence failed: {error}");
        }
    }

    pub(crate) fn save_note(
        &mut self,
        scope: &str,
        key: &str,
        content: &str,
    ) -> Result<ee_mcp::SessionNoteResult, AgentError> {
        validate_note_key(key)?;
        validate_note_content(content)?;
        let notes = self.notes.entry(scope.to_owned()).or_default();
        let replacing = notes.get(key).map_or(0, String::len);
        if !notes.contains_key(key) && notes.len() >= NOTES_PER_SCOPE {
            return Err(AgentError::invalid_params("session note limit reached"));
        }
        let total = notes.values().map(String::len).sum::<usize>();
        if total.saturating_sub(replacing).saturating_add(content.len()) > NOTES_TOTAL_BYTES {
            return Err(AgentError::invalid_params("session note byte limit reached"));
        }
        notes.insert(key.to_owned(), content.to_owned());
        self.persist_notes();
        Ok(note_result(key, content))
    }

    pub(crate) fn read_notes(&self, scope: &str) -> Result<ee_mcp::SessionNotesResult, AgentError> {
        let notes = self.notes.get(scope);
        let total = notes.map_or(0, BTreeMap::len);
        let mut entries = Vec::new();
        if let Some(notes) = notes {
            for (key, content) in notes.iter().take(NOTES_READ_LIMIT) {
                validate_note_content(content)?;
                entries.push(note_result(key, content));
            }
        }
        Ok(ee_mcp::SessionNotesResult {
            notes: entries,
            note_limit: NOTES_READ_LIMIT as u32,
            total_note_count: total as u32,
            omitted_note_count: total.saturating_sub(NOTES_READ_LIMIT) as u32,
            truncated: total > NOTES_READ_LIMIT,
        })
    }

    pub(crate) fn read_note(
        &self,
        scope: &str,
        key: &str,
    ) -> Result<ee_mcp::SessionNoteResult, AgentError> {
        validate_note_key(key)?;
        let content = self
            .notes
            .get(scope)
            .and_then(|notes| notes.get(key))
            .ok_or_else(|| AgentError::invalid_params("session note not found"))?;
        validate_note_content(content)?;
        Ok(note_result(key, content))
    }
}

pub(crate) fn project_instructions(
    root: &Path,
) -> Result<ee_mcp::ProjectInstructionsResult, AgentError> {
    let root = std::fs::canonicalize(root)
        .map_err(|error| AgentError::Io(format!("cannot resolve workspace root: {error}")))?;
    let mut sources = Vec::new();
    for (name, kind, precedence, cap) in [
        ("AGENTS.md", "agent_instructions", 10, INSTRUCTION_BYTES),
        ("RULE.md", "rule", 20, INSTRUCTION_BYTES),
        (".ee.toml", "workspace_config", 30, CONFIG_BYTES),
    ] {
        let path = root.join(name);
        if !path.is_file() {
            continue;
        }
        let canonical = std::fs::canonicalize(&path).map_err(|error| {
            AgentError::Io(format!("cannot resolve workspace guidance: {error}"))
        })?;
        if !canonical.starts_with(&root) {
            return Err(AgentError::invalid_params("workspace guidance path escapes root"));
        }
        let bytes = std::fs::read(&canonical)
            .map_err(|error| AgentError::Io(format!("cannot read workspace guidance: {error}")))?;
        let truncated = bytes.len() > cap;
        let text = String::from_utf8_lossy(&bytes[..bytes.len().min(cap)]).into_owned();
        let content = if kind == "workspace_config" { safe_config_summary(&text) } else { text };
        sources.push(ee_mcp::ProjectInstructionSource {
            path: canonical.display().to_string(),
            kind: kind.to_owned(),
            precedence,
            content,
            truncated,
        });
    }
    Ok(ee_mcp::ProjectInstructionsResult {
        root: root.display().to_string(),
        sources,
        tool_constraints: vec![
            String::from("paths must be absolute and inside configured workspace roots"),
            String::from("tool results are bounded"),
            String::from("session notes are runtime-only and reject secret-like content"),
            String::from(
                "file dependency data is editor-index owned and may be unavailable or stale",
            ),
        ],
        truncated: false,
    })
}

pub(crate) fn unavailable_dependency_map(path: PathBuf) -> ee_mcp::FileDependencyMapResult {
    ee_mcp::FileDependencyMapResult {
        path: path.display().to_string(),
        available: false,
        reason: Some(String::from("editor dependency index is unavailable")),
        freshness: String::from("unavailable"),
        indexed_at: None,
        outgoing: Vec::new(),
        incoming: Vec::new(),
        truncated: false,
    }
}

fn note_result(key: &str, content: &str) -> ee_mcp::SessionNoteResult {
    ee_mcp::SessionNoteResult {
        key: key.to_owned(),
        content: content.to_owned(),
        bytes: content.len() as u32,
        truncated: false,
    }
}

fn validate_note_key(key: &str) -> Result<(), AgentError> {
    let valid = !key.is_empty()
        && key.len() <= 64
        && key.as_bytes()[0].is_ascii_alphanumeric()
        && key
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'));
    if valid {
        Ok(())
    } else {
        Err(AgentError::invalid_params("note key must match [A-Za-z0-9][A-Za-z0-9._-]{0,63}"))
    }
}

fn validate_note_content(content: &str) -> Result<(), AgentError> {
    if content.is_empty() || content.len() > NOTE_BYTES {
        return Err(AgentError::invalid_params("note content must be 1..=16384 bytes"));
    }
    let lowered = content.to_ascii_lowercase();
    let secret_assignment = content.lines().any(|line| {
        let Some((key, _)) = line.split_once(['=', ':']) else { return false };
        ee_agent_host::redact::is_secret_key(key.trim())
    });
    let token_like = ["-----begin ", "bearer ", "api_key", "access_token", "secret_key"]
        .iter()
        .any(|needle| lowered.contains(needle));
    if secret_assignment || token_like {
        Err(AgentError::invalid_params("note content appears to contain a secret"))
    } else {
        Ok(())
    }
}

fn safe_config_summary(content: &str) -> String {
    content
        .lines()
        .filter(|line| {
            let key = line.split_once('=').map_or("", |(key, _)| key.trim());
            !ee_agent_host::redact::is_secret_key(key)
                && !matches!(key, "command" | "args" | "env" | "headers" | "url")
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn notes_reject_secret_assignments_and_stay_scoped() {
        let mut knowledge = ProjectKnowledge::default();
        assert!(knowledge.save_note("one", "plan", "ship it").is_ok());
        assert!(knowledge.save_note("one", "token", "TOKEN=secret").is_err());
        assert!(knowledge.read_note("two", "plan").is_err());
        assert_eq!(knowledge.read_note("one", "plan").expect("note").content, "ship it");
    }

    #[test]
    fn notes_persist_opt_in_roundtrips_through_disk() {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join("agent-notes.json");
        {
            let mut knowledge = ProjectKnowledge::new(Some(path.clone()));
            knowledge.save_note("scope-a", "plan", "ship it").expect("save plan");
            knowledge
                .save_note("scope-a", "decision", "adopt bounded memory")
                .expect("save decision");
        }
        let reloaded = ProjectKnowledge::new(Some(path.clone()));
        assert_eq!(reloaded.read_note("scope-a", "plan").expect("plan").content, "ship it");
        assert_eq!(
            reloaded.read_note("scope-a", "decision").expect("decision").content,
            "adopt bounded memory"
        );
        // Note writes are durable: replacing overwrites and persists.
        let mut updated = ProjectKnowledge::new(Some(path.clone()));
        updated
            .save_note("scope-a", "decision", "reconsider bounded memory")
            .expect("replace note");
        let reloaded_again = ProjectKnowledge::new(Some(path));
        assert_eq!(
            reloaded_again.read_note("scope-a", "decision").expect("decision").content,
            "reconsider bounded memory"
        );
    }

    #[test]
    fn notes_remain_memory_only_without_opt_in() {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join("agent-notes.json");
        let mut knowledge = ProjectKnowledge::new(None);
        knowledge.save_note("scope-a", "plan", "volatile").expect("save");
        assert!(!path.exists(), "default notes never touch disk");
        let reloaded = ProjectKnowledge::new(Some(path.clone()));
        assert!(reloaded.read_note("scope-a", "plan").is_err(), "in-memory notes not persisted");
    }
}
