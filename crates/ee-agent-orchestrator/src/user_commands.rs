//! User-defined slash commands discovered from markdown templates.
//!
//! Roots, in precedence order (workspace wins on name collisions):
//!
//! 1. `<workspace-root>/.agents/commands/**/*.md`
//! 2. `$XDG_CONFIG_HOME/ee/commands/**/*.md`
//!
//! Frontmatter carries `description`, optional `agent`, optional `model`, and
//! `subtask: bool`. The body is a template: `$ARGUMENTS` expands to all
//! argument text, `$1`..`$999` expand positionally, and `$$` is a literal `$`.
//! Shell interpolation is intentionally rejected at discovery until the
//! provider can route it through the terminal approval gate; a template that
//! contains ``!`cmd` `` is skipped with a warning instead of being executed
//! without approval.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use ee_agent_protocol::{
    AvailableCommand, AvailableCommandInput, BRANCH_COMMAND_NAME, COMPACT_COMMAND_NAME,
    DIFF_COMMAND_NAME, DISCARD_COMMAND_NAME, EXPORT_SESSION_COMMAND_NAME, HARNESS_COMMAND_NAME,
    IMPORT_SESSION_COMMAND_NAME, PRIMARY_AGENT_COMMAND_NAME, REFLECT_COMMAND_NAME,
    RESUME_COMMAND_NAME, REVERT_COMMAND_NAME, RUBBER_DUCK_COMMAND_NAME, UnstructuredCommandInput,
};
use serde::{Deserialize, Serialize};

use crate::user_agents::UserAgentScope;

/// Maximum number of commands kept for one session.
pub const MAX_USER_COMMANDS: usize = 64;
/// Maximum command template (markdown body) size.
pub const MAX_COMMAND_TEMPLATE_BYTES: usize = 64 * 1024;
/// Maximum command markdown file size, including frontmatter.
pub const MAX_COMMAND_FILE_BYTES: usize = 128 * 1024;
/// Maximum characters retained in a command description.
pub const MAX_COMMAND_DESCRIPTION_CHARS: usize = 512;
/// Maximum arguments accepted by one command invocation.
pub const MAX_COMMAND_ARGS: usize = 64;
/// Maximum rendered prompt size.
pub const MAX_RENDERED_COMMAND_BYTES: usize = 64 * 1024;
/// Maximum directory depth walked below a command root.
pub const MAX_COMMAND_DIR_DEPTH: usize = 4;
/// Maximum markdown files inspected per command root.
pub const MAX_COMMAND_FILES_SCANNED: usize = 256;

/// Advertised commands whose names user templates cannot shadow.
pub const RESERVED_COMMAND_NAMES: &[&str] = &[
    COMPACT_COMMAND_NAME,
    RUBBER_DUCK_COMMAND_NAME,
    DISCARD_COMMAND_NAME,
    RESUME_COMMAND_NAME,
    BRANCH_COMMAND_NAME,
    REVERT_COMMAND_NAME,
    DIFF_COMMAND_NAME,
    EXPORT_SESSION_COMMAND_NAME,
    IMPORT_SESSION_COMMAND_NAME,
    PRIMARY_AGENT_COMMAND_NAME,
    REFLECT_COMMAND_NAME,
    HARNESS_COMMAND_NAME,
    // The CLI intercepts these before ACP, so a provider-level definition
    // would never run and tab completion would be misleading.
    "agent",
    "model",
];

/// One validated user-defined slash command.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UserCommandDefinition {
    /// Command name without the leading slash.
    pub name: String,
    /// Bounded description shown in completion.
    pub description: String,
    /// Optional agent to select (primary) or use as the subtask role.
    pub agent: Option<String>,
    /// Optional model id applied before prompting or to the subtask role.
    pub model: Option<String>,
    /// Whether the command runs as a child subagent instead of the session.
    pub subtask: bool,
    /// Markdown body template.
    pub template: String,
    /// Definition scope.
    pub scope: UserAgentScope,
    /// Source file path.
    pub path: PathBuf,
}

impl UserCommandDefinition {
    /// Expands `$ARGUMENTS`, `$1`..`$999`, and `$$` in the template.
    pub fn render(&self, arguments: &[String]) -> Result<String, String> {
        if arguments.len() > MAX_COMMAND_ARGS {
            return Err(format!(
                "command `{}` accepts at most {MAX_COMMAND_ARGS} arguments",
                self.name
            ));
        }
        let joined = arguments.join(" ");
        let chars: Vec<char> = self.template.chars().collect();
        let mut rendered = String::new();
        let mut index = 0usize;
        while index < chars.len() {
            if chars[index] != '$' {
                rendered.push(chars[index]);
                index += 1;
                continue;
            }
            let rest: String = chars.iter().skip(index + 1).take(9).collect();
            if rest.starts_with("ARGUMENTS") {
                rendered.push_str(&joined);
                index += 1 + "ARGUMENTS".chars().count();
                continue;
            }
            if chars.get(index + 1) == Some(&'$') {
                rendered.push('$');
                index += 2;
                continue;
            }
            let mut cursor = index + 1;
            let mut position = 0usize;
            let mut digits = 0usize;
            while let Some(digit) = chars.get(cursor).and_then(|character| character.to_digit(10)) {
                if digits == 3 {
                    break;
                }
                position = position * 10 + digit as usize;
                cursor += 1;
                digits += 1;
            }
            if digits > 0 {
                if let Some(argument) = position.checked_sub(1).and_then(|slot| arguments.get(slot))
                {
                    rendered.push_str(argument);
                }
                index = cursor;
                continue;
            }
            rendered.push('$');
            index += 1;
        }
        if rendered.len() > MAX_RENDERED_COMMAND_BYTES {
            return Err(format!(
                "command `{}` expands past the {MAX_RENDERED_COMMAND_BYTES}-byte limit",
                self.name
            ));
        }
        Ok(rendered)
    }

    /// Completion entry advertised to the client.
    #[must_use]
    pub fn available_command(&self) -> AvailableCommand {
        AvailableCommand::new(self.name.clone(), self.description.clone()).input(
            AvailableCommandInput::Unstructured(UnstructuredCommandInput::new("command arguments")),
        )
    }
}

/// Validated registry of user-defined commands.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct UserCommandRegistry {
    commands: BTreeMap<String, UserCommandDefinition>,
    warnings: Vec<String>,
}

impl UserCommandRegistry {
    /// Discovers command templates from every workspace root and the personal
    /// config root. Workspace definitions override personal ones.
    #[must_use]
    pub fn discover(cwd: &Path, workspace_roots: &[PathBuf], config_root: Option<&Path>) -> Self {
        let mut registry = Self::default();
        let mut seen_workspaces = std::collections::BTreeSet::new();
        for workspace in std::iter::once(cwd).chain(workspace_roots.iter().map(PathBuf::as_path)) {
            let workspace =
                std::fs::canonicalize(workspace).unwrap_or_else(|_| workspace.to_path_buf());
            if !seen_workspaces.insert(workspace.clone()) {
                continue;
            }
            registry
                .load_root(&workspace.join(".agents").join("commands"), UserAgentScope::Workspace);
        }
        if let Some(root) = config_root {
            registry.load_root(&root.join("commands"), UserAgentScope::User);
        }
        registry.warnings.sort();
        registry.warnings.dedup();
        registry
    }

    /// Looks up a command by name.
    #[must_use]
    pub fn get(&self, name: &str) -> Option<&UserCommandDefinition> {
        self.commands.get(name)
    }

    /// All commands in name order.
    pub fn iter(&self) -> impl Iterator<Item = &UserCommandDefinition> {
        self.commands.values()
    }

    /// Number of loaded commands.
    #[must_use]
    pub fn len(&self) -> usize {
        self.commands.len()
    }

    /// Whether no command was loaded.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.commands.is_empty()
    }

    /// Bounded discovery warnings.
    #[must_use]
    pub fn warnings(&self) -> &[String] {
        &self.warnings
    }

    /// Completion entries for every loaded command.
    #[must_use]
    pub fn available_commands(&self) -> Vec<AvailableCommand> {
        self.commands.values().map(UserCommandDefinition::available_command).collect()
    }

    fn load_root(&mut self, root: &Path, scope: UserAgentScope) {
        let canonical_root = match std::fs::canonicalize(root) {
            Ok(root) => root,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return,
            Err(error) => {
                self.warn(format!("cannot read command root {}: {error}", root.display()));
                return;
            }
        };
        let mut files = Vec::new();
        crate::markdown_config::collect_markdown(
            &canonical_root,
            0,
            MAX_COMMAND_DIR_DEPTH,
            MAX_COMMAND_FILES_SCANNED,
            &mut files,
        );
        files.sort();
        for file in files {
            if self.commands.len() >= MAX_USER_COMMANDS {
                self.warn(format!(
                    "command limit {MAX_USER_COMMANDS} reached; skipping remaining templates"
                ));
                return;
            }
            match load_command_file(&file, &canonical_root, scope) {
                Ok(command) => {
                    if let Some(existing) = self.commands.get(&command.name)
                        && existing.scope == UserAgentScope::Workspace
                        && command.scope == UserAgentScope::User
                    {
                        self.warn(format!(
                            "ignoring user command {}: project definition wins",
                            command.name
                        ));
                        continue;
                    }
                    if self.commands.contains_key(&command.name) {
                        self.warn(format!(
                            "command {} is defined more than once; using {}",
                            command.name,
                            command.path.display()
                        ));
                    }
                    self.commands.insert(command.name.clone(), command);
                }
                Err(reason) => self.warn(format!("skipping {}: {reason}", file.display())),
            }
        }
    }

    fn warn(&mut self, warning: String) {
        tracing::warn!(%warning, "user command discovery warning");
        self.warnings.push(warning);
    }
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct CommandFrontmatter {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    agent: Option<String>,
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    subtask: bool,
}

fn load_command_file(
    path: &Path,
    scope_root: &Path,
    scope: UserAgentScope,
) -> Result<UserCommandDefinition, String> {
    let (canonical, content) =
        crate::markdown_config::read_bounded(path, scope_root, MAX_COMMAND_FILE_BYTES)?;
    let (frontmatter, body) = crate::markdown_config::split_frontmatter(&content)?;
    let frontmatter: CommandFrontmatter = crate::markdown_config::parse_frontmatter(frontmatter)?;
    let fallback_name = canonical.file_stem().and_then(|stem| stem.to_str()).unwrap_or_default();
    let name = frontmatter
        .name
        .as_deref()
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .unwrap_or(fallback_name);
    crate::markdown_config::validate_identifier(name, "command name")?;
    if RESERVED_COMMAND_NAMES.contains(&name) {
        return Err(format!("command name {name:?} is reserved by a built-in command"));
    }
    let template = body.trim();
    if template.is_empty() {
        return Err("command template is empty".to_string());
    }
    if template.len() > MAX_COMMAND_TEMPLATE_BYTES {
        return Err(format!(
            "template exceeds the {MAX_COMMAND_TEMPLATE_BYTES}-byte command limit"
        ));
    }
    if template.contains("!`") {
        return Err("shell interpolation is not enabled: it requires the terminal approval gate"
            .to_string());
    }
    let description = frontmatter
        .description
        .as_deref()
        .map(str::trim)
        .filter(|description| !description.is_empty())
        .map(|description| {
            crate::markdown_config::truncate_chars(description, MAX_COMMAND_DESCRIPTION_CHARS)
        })
        .unwrap_or_else(|| format!("User-defined command {name}"));
    Ok(UserCommandDefinition {
        name: name.to_string(),
        description,
        agent: non_empty(frontmatter.agent),
        model: non_empty(frontmatter.model),
        subtask: frontmatter.subtask,
        template: template.to_string(),
        scope,
        path: canonical,
    })
}

fn non_empty(value: Option<String>) -> Option<String> {
    value.map(|value| value.trim().to_string()).filter(|value| !value.is_empty())
}

/// Splits argument text into shell-like words: whitespace separates, single
/// and double quotes group, and a backslash escapes the next character outside
/// single quotes. Unterminated quotes fail closed.
pub fn parse_arguments(text: &str) -> Result<Vec<String>, String> {
    let mut arguments = Vec::new();
    let mut current = String::new();
    let mut started = false;
    let mut single = false;
    let mut double = false;
    let mut escaped = false;
    for character in text.chars() {
        if escaped {
            current.push(character);
            escaped = false;
            started = true;
            continue;
        }
        match character {
            '\\' if !single => {
                escaped = true;
                started = true;
            }
            '\'' if !double => {
                single = !single;
                started = true;
            }
            '"' if !single => {
                double = !double;
                started = true;
            }
            character if character.is_whitespace() && !single && !double => {
                if started {
                    arguments.push(std::mem::take(&mut current));
                    started = false;
                }
            }
            character => {
                current.push(character);
                started = true;
            }
        }
        if arguments.len() > MAX_COMMAND_ARGS {
            return Err(format!("at most {MAX_COMMAND_ARGS} arguments are supported"));
        }
    }
    if escaped {
        current.push('\\');
    }
    if single || double {
        return Err("unterminated quoted argument".to_string());
    }
    if started {
        arguments.push(current);
    }
    if arguments.len() > MAX_COMMAND_ARGS {
        return Err(format!("at most {MAX_COMMAND_ARGS} arguments are supported"));
    }
    Ok(arguments)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_command(root: &Path, relative: &str, content: &str) -> PathBuf {
        let path = root.join(relative);
        std::fs::create_dir_all(path.parent().expect("command parent")).expect("create dir");
        std::fs::write(&path, content).expect("write command");
        path
    }

    #[test]
    fn discovers_templates_and_expands_arguments() {
        let temp = tempfile::tempdir().expect("tempdir");
        write_command(
            temp.path(),
            ".agents/commands/release.md",
            "---\ndescription: Prepare a release\nagent: reviewer\nsubtask: true\n---\nRelease $1 and $2. All: $ARGUMENTS. Literal $$.\n",
        );
        let registry = UserCommandRegistry::discover(temp.path(), &[], None);
        let command = registry.get("release").expect("release discovered");
        assert_eq!(command.description, "Prepare a release");
        assert_eq!(command.agent.as_deref(), Some("reviewer"));
        assert!(command.subtask);
        assert_eq!(
            command.render(&["v1".into(), "now".into()]).expect("renders"),
            "Release v1 and now. All: v1 now. Literal $."
        );
    }

    #[test]
    fn workspace_command_overrides_user_command() {
        let workspace = tempfile::tempdir().expect("workspace");
        let user = tempfile::tempdir().expect("user");
        write_command(user.path(), "commands/release.md", "---\n---\nuser $ARGUMENTS\n");
        write_command(
            workspace.path(),
            ".agents/commands/release.md",
            "---\n---\nproject $ARGUMENTS\n",
        );
        let registry = UserCommandRegistry::discover(workspace.path(), &[], Some(user.path()));
        let command = registry.get("release").expect("release discovered");
        assert_eq!(command.scope, UserAgentScope::Workspace);
        assert!(command.template.starts_with("project"));
    }

    #[test]
    fn malformed_and_reserved_commands_are_skipped() {
        let temp = tempfile::tempdir().expect("tempdir");
        write_command(temp.path(), ".agents/commands/compact.md", "---\n---\nbody\n");
        write_command(temp.path(), ".agents/commands/broken.md", "no frontmatter\n");
        write_command(temp.path(), ".agents/commands/shell.md", "---\n---\nrun !`rm -rf target`\n");
        let registry = UserCommandRegistry::discover(temp.path(), &[], None);
        assert!(registry.is_empty());
        assert!(registry.warnings().iter().any(|warning| warning.contains("reserved")));
        assert!(registry.warnings().iter().any(|warning| warning.contains("frontmatter")));
        assert!(registry.warnings().iter().any(|warning| warning.contains("shell interpolation")));
    }

    #[test]
    fn missing_positional_arguments_expand_to_empty() {
        let temp = tempfile::tempdir().expect("tempdir");
        write_command(temp.path(), ".agents/commands/check.md", "---\n---\n[$1][$2]\n");
        let registry = UserCommandRegistry::discover(temp.path(), &[], None);
        let command = registry.get("check").expect("check discovered");
        assert_eq!(command.render(&[]).expect("renders"), "[][]");
        assert_eq!(command.render(&["one".into()]).expect("renders"), "[one][]");
    }

    #[test]
    fn argument_parser_handles_quotes_and_rejects_unterminated_quotes() {
        assert_eq!(
            parse_arguments("one \"two words\" 'three words'").expect("parses"),
            vec!["one", "two words", "three words"]
        );
        assert_eq!(parse_arguments(r#"a\ b"#).expect("parses"), vec!["a b"]);
        assert!(parse_arguments("\"unterminated").is_err());
        assert_eq!(parse_arguments("''").expect("empty quoted arg"), vec![""]);
    }

    #[test]
    fn available_commands_expose_names_and_descriptions() {
        let temp = tempfile::tempdir().expect("tempdir");
        write_command(
            temp.path(),
            ".agents/commands/release.md",
            "---\ndescription: Prepare a release\n---\nbody\n",
        );
        let registry = UserCommandRegistry::discover(temp.path(), &[], None);
        let commands = registry.available_commands();
        assert_eq!(commands.len(), 1);
        assert_eq!(commands[0].name, "release");
        assert_eq!(commands[0].description, "Prepare a release");
    }

    #[test]
    fn rendered_size_is_bounded() {
        let temp = tempfile::tempdir().expect("tempdir");
        write_command(temp.path(), ".agents/commands/huge.md", "---\n---\n$ARGUMENTS\n");
        let registry = UserCommandRegistry::discover(temp.path(), &[], None);
        let command = registry.get("huge").expect("huge discovered");
        let argument = "x".repeat(MAX_RENDERED_COMMAND_BYTES + 1);
        assert!(command.render(&[argument]).is_err());
    }
}
