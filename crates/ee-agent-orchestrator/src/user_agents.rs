//! User-defined agents discovered from markdown files.
//!
//! Discovery roots, in precedence order:
//!
//! 1. `$XDG_CONFIG_HOME/ee/agents/**/*.md` (personal agents)
//! 2. `<workspace-root>/.agents/agents/**/*.md` (project agents)
//!
//! The body of the file is the agent prompt. YAML frontmatter carries the
//! bounded metadata: `name`, `description`, `model`, `variant`, `mode`,
//! `hidden`, `steps`, `permission`, and `disabled`.
//!
//! `mode: primary` agents are selectable for the root session; `mode:
//! subagent` agents join the built-in `delegate_task` role library. Project
//! files are repository-controlled, so their prompt is injected through the
//! untrusted workspace plane for primary agents and is never allowed to grant
//! a tool class the session policy denied.
//!
//! Malformed, oversized, duplicate-by-name, and disabled files are skipped
//! with a bounded warning instead of failing the whole discovery pass.

use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::permission_rules::{PermissionEffect, PermissionRuleset};
use crate::subagents::{SUBAGENT_DEFAULT_MAX_ITERATIONS, SubagentRole};
use crate::tools::SideEffectClass;

/// Maximum number of agents kept for one session after precedence is applied.
pub const MAX_USER_AGENTS: usize = 32;
/// Maximum agent prompt (markdown body) size.
pub const MAX_AGENT_PROMPT_BYTES: usize = 32 * 1024;
/// Maximum agent markdown file size, including frontmatter.
pub const MAX_AGENT_FILE_BYTES: usize = 96 * 1024;
/// Maximum characters retained in an agent description.
pub const MAX_AGENT_DESCRIPTION_CHARS: usize = 512;
/// Maximum directory depth walked below an agent root.
pub const MAX_AGENT_DIR_DEPTH: usize = 4;
/// Maximum markdown files inspected per agent root. Keeps discovery bounded
/// even when the root contains far more files than the 32-agent registry cap.
pub const MAX_AGENT_FILES_SCANNED: usize = 256;
/// Maximum accepted `steps` budget; larger values are rejected as malformed.
pub const MAX_AGENT_STEPS: usize = 64;

/// Where an agent definition came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UserAgentScope {
    /// `$XDG_CONFIG_HOME/ee/agents`.
    User,
    /// `<workspace>/.agents/agents`.
    Workspace,
}

/// How a user-defined agent participates in a session.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UserAgentMode {
    /// Selectable as the root session agent.
    Primary,
    /// Available as a `delegate_task` role.
    Subagent,
}

impl UserAgentMode {
    /// Stable lowercase label used in diagnostics.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Primary => "primary",
            Self::Subagent => "subagent",
        }
    }
}

/// One validated user-defined agent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UserAgentDefinition {
    /// Stable identifier used by `/primary-agent` and `delegate_task.role_name`.
    pub name: String,
    /// Bounded human-readable description.
    pub description: String,
    /// Registry model id, when the agent pins a model.
    pub model: Option<String>,
    /// Optional model variant selector.
    pub variant: Option<String>,
    /// Primary or subagent mode.
    pub mode: UserAgentMode,
    /// Hidden agents stay selectable by name but are omitted from listings.
    pub hidden: bool,
    /// Loop/role step budget, when the agent pins one.
    pub steps: Option<usize>,
    /// Markdown body used as the agent prompt.
    pub prompt: String,
    /// Optional per-agent permission overlay.
    pub permissions: Option<PermissionRuleset>,
    /// Definition scope.
    pub scope: UserAgentScope,
    /// Source file path.
    pub path: PathBuf,
}

impl UserAgentDefinition {
    /// Registry model ids to try, most specific first.
    ///
    /// The `variant` is encoded as the provider-neutral `model#variant`
    /// selector; callers fall back to the bare model id when the registry
    /// does not carry the variant.
    #[must_use]
    pub fn model_candidates(&self) -> Vec<String> {
        match (&self.model, &self.variant) {
            (Some(model), Some(variant)) => vec![format!("{model}#{variant}"), model.clone()],
            (Some(model), None) => vec![model.clone()],
            _ => Vec::new(),
        }
    }

    /// Whether this agent can be selected for the root session.
    #[must_use]
    pub fn is_primary(&self) -> bool {
        self.mode == UserAgentMode::Primary
    }

    /// Whether this agent joins the delegation role library.
    #[must_use]
    pub fn is_subagent(&self) -> bool {
        self.mode == UserAgentMode::Subagent
    }

    /// Tool classes the agent may use.
    ///
    /// The overlay is deny-biased: an explicit `allow`/`ask` for a class
    /// pattern selects that class, an explicit `deny` removes it, and the
    /// session policy still gates the final decision. A definition can never
    /// expand beyond the class set it names.
    #[must_use]
    pub fn tool_classes(&self) -> Vec<SideEffectClass> {
        let mut classes = vec![SideEffectClass::Read];
        let Some(rules) = &self.permissions else {
            return classes;
        };
        for rule in rules.rules() {
            let targets = class_targets(&rule.action);
            for class in targets {
                match rule.effect {
                    PermissionEffect::Allow | PermissionEffect::Ask => {
                        if !classes.contains(&class) {
                            classes.push(class);
                        }
                    }
                    PermissionEffect::Deny => {
                        if rule.resource == "*" {
                            classes.retain(|existing| *existing != class);
                        }
                    }
                }
            }
        }
        classes.sort_unstable();
        classes.dedup();
        classes
    }

    /// Builds the subagent role for `delegate_task`.
    ///
    /// Per-agent permission rules travel with the role and are enforced by
    /// the child policy before any tool dispatch or host approval request.
    #[must_use]
    pub fn to_subagent_role(&self) -> SubagentRole {
        SubagentRole {
            name: self.name.clone(),
            instructions: format!(
                "You are the user-defined agent `{}`.\n\n{}",
                self.name, self.prompt
            ),
            allowed_tool_classes: self.tool_classes(),
            max_iterations: self
                .steps
                .filter(|steps| *steps > 0)
                .unwrap_or(SUBAGENT_DEFAULT_MAX_ITERATIONS),
            allowed_scope_globs: Vec::new(),
            model: self.model_candidates().into_iter().next(),
            requires_evidence: true,
            workspace_isolation: crate::workspace_isolation::WorkspaceIsolation::Shared,
            permission_rules: self.permissions.clone(),
        }
    }
}

/// Maps a permission action pattern to the tool classes it names.
fn class_targets(action: &str) -> Vec<SideEffectClass> {
    let action = action.trim().to_ascii_lowercase();
    let action = action.strip_prefix("class:").unwrap_or(&action);
    match action {
        "*" => {
            // Wildcards grant the ordinary classes. Delegation stays explicit so
            // a repository-provided agent file cannot silently recurse.
            vec![SideEffectClass::Read, SideEffectClass::Write, SideEffectClass::Execute]
        }
        "read" | "search" | "fetch" => vec![SideEffectClass::Read],
        "write" | "edit" | "delete" | "move" | "create" => vec![SideEffectClass::Write],
        "execute" | "bash" | "terminal" | "shell" => vec![SideEffectClass::Execute],
        "delegate" | "subagent" => vec![SideEffectClass::Delegate],
        _ => Vec::new(),
    }
}

/// Validated registry of user-defined agents.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct UserAgentRegistry {
    agents: std::collections::BTreeMap<String, UserAgentDefinition>,
    warnings: Vec<String>,
}

impl UserAgentRegistry {
    /// Discovers agents from the personal config root and every workspace
    /// root. Workspace definitions override personal ones with the same name.
    ///
    /// Discovery is bounded and never fails: unreadable roots contribute
    /// nothing, and malformed files become warnings.
    #[must_use]
    pub fn discover(cwd: &Path, workspace_roots: &[PathBuf], config_root: Option<&Path>) -> Self {
        let mut registry = Self::default();
        let mut seen_workspaces = std::collections::BTreeSet::new();
        // Workspace roots load first so personal agents cannot consume the
        // 32-agent budget before higher-precedence project files are seen.
        for workspace in std::iter::once(cwd).chain(workspace_roots.iter().map(PathBuf::as_path)) {
            let workspace = fs::canonicalize(workspace).unwrap_or_else(|_| workspace.to_path_buf());
            if !seen_workspaces.insert(workspace.clone()) {
                continue;
            }
            registry
                .load_root(&workspace.join(".agents").join("agents"), UserAgentScope::Workspace);
        }
        if let Some(root) = config_root {
            registry.load_root(&root.join("agents"), UserAgentScope::User);
        }
        registry.warnings.sort();
        registry.warnings.dedup();
        registry
    }

    /// All loaded agents keyed by name.
    #[must_use]
    pub fn get(&self, name: &str) -> Option<&UserAgentDefinition> {
        self.agents.get(name)
    }

    /// Primary-mode agents in name order.
    pub fn primary(&self) -> impl Iterator<Item = &UserAgentDefinition> {
        self.agents.values().filter(|agent| agent.is_primary())
    }

    /// Non-hidden primary-mode agents in name order, for listings.
    pub fn selectable_primary(&self) -> impl Iterator<Item = &UserAgentDefinition> {
        self.primary().filter(|agent| !agent.hidden)
    }

    /// Subagent-mode agents in name order.
    pub fn subagent(&self) -> impl Iterator<Item = &UserAgentDefinition> {
        self.agents.values().filter(|agent| agent.is_subagent())
    }

    /// All loaded agents in name order.
    pub fn iter(&self) -> impl Iterator<Item = &UserAgentDefinition> {
        self.agents.values()
    }

    /// Number of loaded agents.
    #[must_use]
    pub fn len(&self) -> usize {
        self.agents.len()
    }

    /// Whether no agent was loaded.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.agents.is_empty()
    }

    /// Bounded discovery warnings, sorted and deduplicated.
    #[must_use]
    pub fn warnings(&self) -> &[String] {
        &self.warnings
    }

    fn load_root(&mut self, root: &Path, scope: UserAgentScope) {
        let canonical_root = match fs::canonicalize(root) {
            Ok(root) => root,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return,
            Err(error) => {
                self.warn(format!("cannot read agent root {}: {error}", root.display()));
                return;
            }
        };
        let mut files = Vec::new();
        crate::markdown_config::collect_markdown(
            &canonical_root,
            0,
            MAX_AGENT_DIR_DEPTH,
            MAX_AGENT_FILES_SCANNED,
            &mut files,
        );
        files.sort();
        for file in files {
            if self.agents.len() >= MAX_USER_AGENTS {
                self.warn(format!(
                    "agent limit {MAX_USER_AGENTS} reached; skipping remaining definitions"
                ));
                return;
            }
            match load_agent_file(&file, &canonical_root, scope) {
                Ok(Some(agent)) => {
                    if let Some(existing) = self.agents.get(&agent.name)
                        && existing.scope == UserAgentScope::Workspace
                        && agent.scope == UserAgentScope::User
                    {
                        self.warn(format!(
                            "ignoring user agent {}: project definition wins",
                            agent.name
                        ));
                        continue;
                    }
                    if self.agents.contains_key(&agent.name) {
                        self.warn(format!(
                            "agent {} is defined more than once; using {}",
                            agent.name,
                            agent.path.display()
                        ));
                    }
                    self.agents.insert(agent.name.clone(), agent);
                }
                Ok(None) => {}
                Err(reason) => self.warn(format!("skipping {}: {reason}", file.display())),
            }
        }
    }

    fn warn(&mut self, warning: String) {
        tracing::warn!(%warning, "user agent discovery warning");
        self.warnings.push(warning);
    }
}

/// Reads every `*.md` file below `root`, bounded by depth.
fn load_agent_file(
    path: &Path,
    scope_root: &Path,
    scope: UserAgentScope,
) -> Result<Option<UserAgentDefinition>, String> {
    let (canonical, content) =
        crate::markdown_config::read_bounded(path, scope_root, MAX_AGENT_FILE_BYTES)?;
    let (frontmatter, body) = crate::markdown_config::split_frontmatter(&content)?;
    let frontmatter: AgentFrontmatter = crate::markdown_config::parse_frontmatter(frontmatter)?;
    if frontmatter.disabled {
        return Ok(None);
    }
    let fallback_name = canonical.file_stem().and_then(|stem| stem.to_str()).unwrap_or_default();
    let name = frontmatter
        .name
        .as_deref()
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .unwrap_or(fallback_name);
    crate::markdown_config::validate_identifier(name, "agent name")?;
    let prompt = body.trim();
    if prompt.is_empty() {
        return Err("agent body is empty".to_string());
    }
    if prompt.len() > MAX_AGENT_PROMPT_BYTES {
        return Err(format!("prompt exceeds the {MAX_AGENT_PROMPT_BYTES}-byte agent limit"));
    }
    let steps = match frontmatter.steps {
        Some(0) => return Err("steps must be at least 1".to_string()),
        Some(steps) if steps > MAX_AGENT_STEPS => {
            return Err(format!("steps must be at most {MAX_AGENT_STEPS}"));
        }
        steps => steps,
    };
    let permissions = match frontmatter.permission {
        Some(value) => Some(
            PermissionRuleset::from_value(&value)
                .map_err(|error| format!("invalid permission rules: {error}"))?,
        ),
        None => None,
    };
    let description = frontmatter
        .description
        .as_deref()
        .map(str::trim)
        .filter(|description| !description.is_empty())
        .map(|description| {
            crate::markdown_config::truncate_chars(description, MAX_AGENT_DESCRIPTION_CHARS)
        })
        .unwrap_or_else(|| {
            format!(
                "User-defined {} agent {name}",
                frontmatter.mode.unwrap_or(UserAgentMode::Primary).as_str()
            )
        });
    Ok(Some(UserAgentDefinition {
        name: name.to_string(),
        description,
        model: non_empty(frontmatter.model),
        variant: non_empty(frontmatter.variant),
        mode: frontmatter.mode.unwrap_or(UserAgentMode::Primary),
        hidden: frontmatter.hidden,
        steps,
        prompt: prompt.to_string(),
        permissions,
        scope,
        path: canonical,
    }))
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct AgentFrontmatter {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    variant: Option<String>,
    #[serde(default)]
    mode: Option<UserAgentMode>,
    #[serde(default)]
    hidden: bool,
    #[serde(default)]
    steps: Option<usize>,
    #[serde(default)]
    disabled: bool,
    #[serde(default, alias = "permissions")]
    permission: Option<serde_json::Value>,
}

fn non_empty(value: Option<String>) -> Option<String> {
    value.map(|value| value.trim().to_string()).filter(|value| !value.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_agent(root: &Path, relative: &str, content: &str) -> PathBuf {
        let path = root.join(relative);
        fs::create_dir_all(path.parent().expect("agent parent")).expect("create agent dir");
        fs::write(&path, content).expect("write agent");
        path
    }

    const REVIEWER: &str = "---\ndescription: Project reviewer\nmode: subagent\nsteps: 3\nmodel: sonnet\nvariant: high\npermission:\n  read: allow\n  edit: deny\n---\nReview the assigned change and cite files.\n";

    #[test]
    fn discovers_project_subagent_and_body_becomes_instructions() {
        let temp = tempfile::tempdir().expect("tempdir");
        write_agent(temp.path(), ".agents/agents/reviewer.md", REVIEWER);
        let registry = UserAgentRegistry::discover(temp.path(), &[], None);
        assert!(registry.get("reviewer").is_some(), "warnings: {:?}", registry.warnings());
        let agent = registry.get("reviewer").expect("reviewer discovered");
        assert_eq!(agent.scope, UserAgentScope::Workspace);
        assert_eq!(agent.mode, UserAgentMode::Subagent);
        assert_eq!(agent.description, "Project reviewer");
        assert_eq!(agent.steps, Some(3));
        assert_eq!(agent.model_candidates(), vec!["sonnet#high", "sonnet"]);
        assert_eq!(agent.tool_classes(), vec![SideEffectClass::Read]);

        let role = agent.to_subagent_role();
        assert_eq!(role.name, "reviewer");
        assert!(role.instructions.contains("Review the assigned change"));
        assert_eq!(role.max_iterations, 3);
        assert_eq!(role.model.as_deref(), Some("sonnet#high"));
        assert!(role.permission_rules.is_some());
        assert!(role.requires_evidence, "user agents fail closed on evidence");
    }

    #[test]
    fn workspace_agent_overrides_user_agent_with_same_name() {
        let workspace = tempfile::tempdir().expect("workspace");
        let user = tempfile::tempdir().expect("user");
        write_agent(user.path(), "agents/reviewer.md", REVIEWER);
        write_agent(
            workspace.path(),
            ".agents/agents/reviewer.md",
            "---\ndescription: Workspace reviewer\nmode: subagent\n---\nWorkspace body.\n",
        );
        let registry = UserAgentRegistry::discover(workspace.path(), &[], Some(user.path()));
        let agent = registry.get("reviewer").expect("reviewer discovered");
        assert_eq!(agent.scope, UserAgentScope::Workspace);
        assert_eq!(agent.description, "Workspace reviewer");
        assert!(agent.prompt.contains("Workspace body"));
    }

    #[test]
    fn disabled_agent_is_removed() {
        let temp = tempfile::tempdir().expect("tempdir");
        write_agent(
            temp.path(),
            ".agents/agents/disabled.md",
            "---\ndisabled: true\n---\nnever used\n",
        );
        let registry = UserAgentRegistry::discover(temp.path(), &[], None);
        assert!(registry.is_empty());
    }

    #[test]
    fn malformed_frontmatter_is_skipped_with_a_warning() {
        let temp = tempfile::tempdir().expect("tempdir");
        write_agent(temp.path(), ".agents/agents/broken.md", "---\nmode: sidekick\n---\nbody\n");
        let registry = UserAgentRegistry::discover(temp.path(), &[], None);
        assert!(registry.is_empty());
        assert_eq!(registry.warnings().len(), 1);
        assert!(registry.warnings()[0].contains("invalid frontmatter"));
    }

    #[test]
    fn oversized_prompt_is_skipped() {
        let temp = tempfile::tempdir().expect("tempdir");
        let body = "x".repeat(MAX_AGENT_PROMPT_BYTES + 1);
        write_agent(
            temp.path(),
            ".agents/agents/huge.md",
            &format!("---\nmode: subagent\n---\n{body}"),
        );
        let registry = UserAgentRegistry::discover(temp.path(), &[], None);
        assert!(registry.is_empty());
        assert!(registry.warnings()[0].contains("prompt exceeds"));
    }

    #[test]
    fn agent_count_is_bounded() {
        let temp = tempfile::tempdir().expect("tempdir");
        for index in 0..(MAX_USER_AGENTS + 2) {
            write_agent(
                temp.path(),
                &format!(".agents/agents/agent-{index}.md"),
                "---\nmode: subagent\n---\nbody\n",
            );
        }
        let registry = UserAgentRegistry::discover(temp.path(), &[], None);
        assert_eq!(registry.len(), MAX_USER_AGENTS);
    }

    #[test]
    fn permission_rules_grant_named_classes_but_never_beyond_them() {
        let temp = tempfile::tempdir().expect("tempdir");
        write_agent(
            temp.path(),
            ".agents/agents/builder.md",
            "---\nmode: subagent\npermission:\n  read: allow\n  edit: ask\n  bash: allow\n  delegate: deny\n---\nbody\n",
        );
        let registry = UserAgentRegistry::discover(temp.path(), &[], None);
        let agent = registry.get("builder").expect("builder discovered");
        assert_eq!(
            agent.tool_classes(),
            vec![SideEffectClass::Read, SideEffectClass::Write, SideEffectClass::Execute]
        );
    }

    #[test]
    fn resource_specific_rules_parse_and_stay_ordered() {
        let temp = tempfile::tempdir().expect("tempdir");
        write_agent(
            temp.path(),
            ".agents/agents/careful.md",
            "---\nmode: subagent\npermission:\n  bash:\n    \"git status\": allow\n    \"rm *\": deny\n---\nbody\n",
        );
        let registry = UserAgentRegistry::discover(temp.path(), &[], None);
        let agent = registry.get("careful").expect("careful discovered");
        let rules = agent.permissions.as_ref().expect("rules parsed");
        assert_eq!(rules.rules().len(), 2);
        assert!(agent.tool_classes().contains(&SideEffectClass::Execute));
    }

    #[test]
    fn primary_agents_default_mode_and_hidden_filter() {
        let temp = tempfile::tempdir().expect("tempdir");
        write_agent(temp.path(), ".agents/agents/visible.md", "---\n---\nbody\n");
        write_agent(temp.path(), ".agents/agents/secret.md", "---\nhidden: true\n---\nbody\n");
        let registry = UserAgentRegistry::discover(temp.path(), &[], None);
        assert_eq!(registry.primary().count(), 2);
        assert_eq!(registry.selectable_primary().count(), 1);
        assert_eq!(registry.selectable_primary().next().expect("visible").name, "visible");
    }

    #[test]
    fn path_escape_is_rejected() {
        let temp = tempfile::tempdir().expect("tempdir");
        let outside = temp.path().join("outside.md");
        fs::write(&outside, "---\n---\nbody\n").expect("write outside");
        let root = temp.path().join("root");
        fs::create_dir_all(&root).expect("root dir");
        let error = load_agent_file(&outside, &root, UserAgentScope::Workspace)
            .expect_err("outside path rejected");
        assert!(error.contains("path escapes the configuration root"));
    }
}
