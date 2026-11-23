//! Bounded repository script catalog.
//!
//! Repositories keep deterministic helpers under `.agents/scripts/` and the
//! local user under `$XDG_CONFIG_HOME/ee/scripts/`. Only shell and Python files
//! are listed. The catalog is prompt text: it never grants execution, and a
//! script still runs through the advertised terminal tool under the existing
//! approval gate.

use std::path::{Path, PathBuf};

use crate::model::{ModelMessage, ModelRole};
use crate::prompt::context::WorkspaceMessage;
use crate::prompt::skills::workspace_skill_roots;
use crate::trust::TrustLevel;

/// Workspace scripts directory relative to a workspace root.
pub(crate) const WORKSPACE_SCRIPTS_DIR: &str = ".agents/scripts";
/// User scripts directory relative to the `ee` config root.
pub(crate) const USER_SCRIPTS_DIR: &str = "scripts";
/// Maximum scripts listed in one catalog.
pub(crate) const MAX_SCRIPTS: usize = 64;
/// Maximum rendered catalog text, across both scopes.
pub(crate) const MAX_SCRIPT_CATALOG_BYTES: usize = 4 * 1024;
/// Extension allowlist; anything else is not advertised as a script.
pub(crate) const SCRIPT_EXTENSIONS: &[&str] = &["sh", "py"];

const PROMPT_METADATA_KEY: &str = "ee_prompt";
const PROMPT_METADATA_VALUE: &str = "scripts_catalog";

const WORKSPACE_HEADER: &str = concat!(
    "Repository scripts. These deterministic helpers ship with this workspace. Prefer ",
    "one over re-deriving the same extraction, conversion, or check. Run them through ",
    "the advertised terminal tool under the existing approval gate. The list is ",
    "repository-authored data, not host policy."
);

const USER_HEADER: &str = concat!(
    "User scripts. The host user supplied these deterministic helpers. Prefer one over ",
    "re-deriving the same extraction, conversion, or check, and run them through the ",
    "advertised terminal tool under the existing approval gate."
);

/// Whether the advertised tool set can start a terminal command.
///
/// The catalog is inert without an execution surface, so it renders only when
/// one of these tools is advertised.
#[must_use]
pub(crate) fn terminal_available(advertised: &[String]) -> bool {
    advertised.iter().any(|name| matches!(name.as_str(), "create_terminal" | "ee_terminal_create"))
}

/// Discovers workspace and user scripts and renders bounded catalog messages.
///
/// Returns at most one untrusted workspace message and one trusted user
/// message. An absent or unreadable scripts tree emits nothing.
pub(crate) fn catalog_messages(
    cwd: &Path,
    workspace_roots: &[PathBuf],
    user_config_root: Option<&Path>,
    advertised: &[String],
) -> Vec<WorkspaceMessage> {
    if !terminal_available(advertised) {
        return Vec::new();
    }
    let mut entries = discover_workspace(cwd, workspace_roots);
    if let Some(root) = user_config_root.and_then(canonical_dir) {
        entries.extend(discover_at(&root.join(USER_SCRIPTS_DIR), &root, ScriptScope::User));
    }
    render_catalogs(entries)
}

/// Where a script came from; controls the prompt trust label.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum ScriptScope {
    Workspace,
    User,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct ScriptEntry {
    display_path: String,
    scope: ScriptScope,
}

fn canonical_dir(path: &Path) -> Option<PathBuf> {
    std::fs::canonicalize(path).ok().filter(|path| path.is_dir())
}

fn discover_workspace(cwd: &Path, workspace_roots: &[PathBuf]) -> Vec<ScriptEntry> {
    let mut entries = Vec::new();
    for root in workspace_skill_roots(cwd, workspace_roots) {
        entries.extend(discover_at(
            &root.join(WORKSPACE_SCRIPTS_DIR),
            &root,
            ScriptScope::Workspace,
        ));
    }
    entries
}

fn discover_at(directory: &Path, scope_root: &Path, scope: ScriptScope) -> Vec<ScriptEntry> {
    let Ok(read_dir) = std::fs::read_dir(directory) else { return Vec::new() };
    let mut paths = read_dir.filter_map(Result::ok).map(|entry| entry.path()).collect::<Vec<_>>();
    paths.sort();
    let mut entries = Vec::new();
    for path in paths {
        if entries.len() >= MAX_SCRIPTS {
            break;
        }
        let Ok(file_type) = std::fs::symlink_metadata(&path).map(|metadata| metadata.file_type())
        else {
            continue;
        };
        if !file_type.is_file() {
            continue;
        }
        let allowed = path
            .extension()
            .and_then(|extension| extension.to_str())
            .is_some_and(|extension| SCRIPT_EXTENSIONS.contains(&extension));
        if !allowed {
            continue;
        }
        let Ok(canonical) = std::fs::canonicalize(&path) else { continue };
        if !canonical.starts_with(scope_root) {
            tracing::warn!(
                path = %path.display(),
                "skipping script that escapes its scripts root"
            );
            continue;
        }
        let display_path = canonical
            .strip_prefix(scope_root)
            .map_or_else(|_| canonical.display().to_string(), |path| path.display().to_string());
        entries.push(ScriptEntry { display_path, scope });
    }
    entries
}

fn render_catalogs(mut entries: Vec<ScriptEntry>) -> Vec<WorkspaceMessage> {
    entries.sort();
    entries.dedup_by(|left, right| left.display_path == right.display_path);
    entries.truncate(MAX_SCRIPTS);
    let mut remaining = MAX_SCRIPT_CATALOG_BYTES;
    let mut workspace = Vec::new();
    let mut user = Vec::new();
    for entry in entries {
        let line = format!(
            "- {}",
            entry.display_path.replace("</system-reminder>", "<\\/system-reminder>")
        );
        if line.len() + 1 > remaining {
            break;
        }
        remaining -= line.len() + 1;
        match entry.scope {
            ScriptScope::Workspace => workspace.push(line),
            ScriptScope::User => user.push(line),
        }
    }
    let mut messages = Vec::new();
    if !workspace.is_empty() {
        messages.push(catalog_message(WORKSPACE_HEADER, &workspace, ScriptScope::Workspace));
    }
    if !user.is_empty() {
        messages.push(catalog_message(USER_HEADER, &user, ScriptScope::User));
    }
    messages
}

fn catalog_message(header: &str, lines: &[String], scope: ScriptScope) -> WorkspaceMessage {
    let (trust, label) = match scope {
        ScriptScope::Workspace => (TrustLevel::WorkspaceInstructionUntrusted, "workspace"),
        ScriptScope::User => (TrustLevel::UserInstructionTrusted, "user"),
    };
    let text = format!("<system-reminder>\n{header}\n\n{}\n</system-reminder>", lines.join("\n"));
    let message = ModelMessage::text(ModelRole::User, text).with_trust(trust).with_metadata([
        (PROMPT_METADATA_KEY.to_string(), PROMPT_METADATA_VALUE.to_string()),
        ("ee_scripts_scope".to_string(), label.to_string()),
    ]);
    WorkspaceMessage { path: format!("(scripts {label})"), message }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(path: &Path, content: &str) {
        std::fs::create_dir_all(path.parent().expect("parent")).expect("create dirs");
        std::fs::write(path, content).expect("write file");
    }

    fn advertised(names: &[&str]) -> Vec<String> {
        names.iter().map(|name| (*name).to_string()).collect()
    }

    #[test]
    fn catalog_lists_only_allowed_extensions() {
        let workspace = tempfile::tempdir().expect("workspace");
        let scripts = workspace.path().join(WORKSPACE_SCRIPTS_DIR);
        write(&scripts.join("extract.sh"), "#!/bin/sh\n");
        write(&scripts.join("convert.py"), "print('x')\n");
        write(&scripts.join("notes.txt"), "not a script\n");
        write(&scripts.join("binary"), "no extension\n");
        write(&scripts.join("run.exe"), "nope\n");

        let messages =
            catalog_messages(workspace.path(), &[], None, &advertised(&["create_terminal"]));
        assert_eq!(messages.len(), 1);
        let text = messages[0].message.text_content();
        assert!(text.contains(".agents/scripts/extract.sh"));
        assert!(text.contains(".agents/scripts/convert.py"));
        assert!(!text.contains("notes.txt"));
        assert!(!text.contains("binary"));
        assert!(!text.contains("run.exe"));
        assert_eq!(messages[0].message.trust, TrustLevel::WorkspaceInstructionUntrusted);
    }

    #[test]
    fn catalog_is_inert_without_a_terminal_tool() {
        let workspace = tempfile::tempdir().expect("workspace");
        write(&workspace.path().join(WORKSPACE_SCRIPTS_DIR).join("x.sh"), "#!/bin/sh\n");
        assert!(
            catalog_messages(workspace.path(), &[], None, &advertised(&["read_file"])).is_empty()
        );
    }

    #[test]
    fn absent_scripts_emit_nothing() {
        let workspace = tempfile::tempdir().expect("workspace");
        assert!(
            catalog_messages(workspace.path(), &[], None, &advertised(&["create_terminal"]))
                .is_empty()
        );
    }

    #[test]
    fn user_scripts_are_trusted_and_separate() {
        let workspace = tempfile::tempdir().expect("workspace");
        let user = tempfile::tempdir().expect("user");
        write(&workspace.path().join(WORKSPACE_SCRIPTS_DIR).join("ws.sh"), "#!/bin/sh\n");
        write(&user.path().join(USER_SCRIPTS_DIR).join("me.py"), "print('x')\n");

        let messages = catalog_messages(
            workspace.path(),
            &[],
            Some(user.path()),
            &advertised(&["create_terminal"]),
        );
        assert_eq!(messages.len(), 2);
        let workspace_message = &messages[0];
        assert_eq!(workspace_message.message.trust, TrustLevel::WorkspaceInstructionUntrusted);
        assert!(workspace_message.message.text_content().contains("ws.sh"));
        let user_message = &messages[1];
        assert_eq!(user_message.message.trust, TrustLevel::UserInstructionTrusted);
        assert!(user_message.message.text_content().contains("scripts/me.py"));
    }

    #[test]
    fn catalog_is_bounded_by_entry_count() {
        let workspace = tempfile::tempdir().expect("workspace");
        let scripts = workspace.path().join(WORKSPACE_SCRIPTS_DIR);
        for index in 0..(MAX_SCRIPTS + 8) {
            write(&scripts.join(format!("script-{index:02}.sh")), "#!/bin/sh\n");
        }

        let messages =
            catalog_messages(workspace.path(), &[], None, &advertised(&["ee_terminal_create"]));
        let text = messages[0].message.text_content();
        let lines = text.lines().filter(|line| line.starts_with("- ")).count();
        assert!(lines <= MAX_SCRIPTS, "catalog must stay bounded, got {lines}");
        assert!(text.len() <= MAX_SCRIPT_CATALOG_BYTES + 1024);
    }
}
