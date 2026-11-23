//! Pure validation for a candidate harness component tree.
//!
//! An improver proposes a tree of component files (guardrails, tool guidance,
//! middleware, scripts, skills, agents, memories). Before any of it can be
//! adopted or written to the experience library, this module applies the
//! paper's discard rules:
//!
//! - every skill must be triggered by a line in always-on text; a missing
//!   trigger line is repaired by appending one and recorded, never silent;
//! - scripts referenced by guidance or skills must exist, use an allowed
//!   extension, and stay inside the component surface;
//! - skill/agent/memory frontmatter must parse and carry valid types;
//! - names must be safe identifiers, must not shadow builtin roles, and must
//!   not duplicate another component in the same tree;
//! - always-on text has a hard character budget, and every verdict reports the
//!   delta against a base tree so composition sees what prose costs.
//!
//! The validator is pure: it reads and returns an in-memory [`HarnessTree`] and
//! never writes files. Script syntax checks are separate functions so only the
//! driver runs processes, never an improver session.

use std::collections::{BTreeMap, BTreeSet};
use std::io::Read as _;
use std::path::{Component, Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use crate::markdown_config;

/// Maximum files accepted in one candidate tree.
pub const MAX_TREE_FILES: usize = 256;
/// Maximum bytes accepted from one candidate file.
pub const MAX_TREE_FILE_BYTES: usize = 64 * 1024;
/// Maximum directory depth accepted in one candidate tree.
pub const MAX_TREE_DEPTH: usize = 4;
/// Hard budget for always-on text, in characters.
///
/// Every always-on character competes with the agent's output budget on every
/// turn, so the editable harness layer is capped conservatively.
pub const MAX_ALWAYS_ON_CHARS: usize = 32_000;
/// Script extension allowlist for harness components.
pub const SCRIPT_EXTENSIONS: &[&str] = &["sh", "py"];
/// Wall-clock bound for one script syntax check.
pub const SYNTAX_CHECK_TIMEOUT: Duration = Duration::from_secs(5);
/// Output bound retained from a failing syntax check.
pub const SYNTAX_CHECK_OUTPUT_BYTES: usize = 4 * 1024;

const MAX_COMPONENT_NAME_CHARS: usize = 64;
const MAX_DESCRIPTION_CHARS: usize = 160;

const ROOT_FILES: &[&str] = &["guardrails.md", "tool_guidance.md", "middleware.toml"];
const COMPONENT_DIRS: &[&str] = &["scripts", "skills", "agents", "memories"];

/// In-memory candidate harness tree.
///
/// Keys are `/`-separated paths relative to the component root (for example
/// `guardrails.md` or `skills/atlas/SKILL.md`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct HarnessTree {
    /// Relative path to file content.
    pub files: BTreeMap<String, String>,
}

impl HarnessTree {
    /// Builds a tree from an explicit file map.
    #[must_use]
    pub fn new(files: BTreeMap<String, String>) -> Self {
        Self { files }
    }

    /// Whether the tree has no files at all.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.files.is_empty()
    }

    /// Loads a bounded component tree from disk.
    ///
    /// Only regular files are read; symlinks, non-UTF-8 files, oversized files,
    /// and over-deep trees fail closed with a named error.
    pub fn load(root: &Path) -> Result<Self, String> {
        let root = std::fs::canonicalize(root)
            .map_err(|error| format!("cannot resolve harness root: {error}"))?;
        if !root.is_dir() {
            return Err(format!("harness root is not a directory: {}", root.display()));
        }
        let mut files = BTreeMap::new();
        collect_files(&root, &root, 0, &mut files)?;
        Ok(Self { files })
    }

    /// Loads only the editable component surface from `.agents`.
    ///
    /// Experience entries and unrelated files are ignored, so a saved library
    /// copy never nests the library inside itself.
    pub fn load_components(root: &Path) -> Result<Self, String> {
        if !root.is_dir() {
            return Ok(Self::default());
        }
        let root = std::fs::canonicalize(root)
            .map_err(|error| format!("cannot resolve harness root: {error}"))?;
        let mut files = BTreeMap::new();
        for file in ROOT_FILES {
            let path = root.join(file);
            if path.is_file() {
                load_one_file(&root, &path, &mut files)?;
            }
        }
        for directory in COMPONENT_DIRS {
            let path = root.join(directory);
            if path.is_dir() {
                collect_files(&root, &path, 1, &mut files)?;
            }
        }
        Ok(Self { files })
    }

    /// Concatenated always-on text: guardrails, tool guidance, and primary
    /// agent bodies.
    fn always_on_text(&self) -> String {
        let mut text = String::new();
        for file in ["guardrails.md", "tool_guidance.md"] {
            if let Some(content) = self.files.get(file) {
                text.push_str(content);
                text.push('\n');
            }
        }
        for (path, content) in &self.files {
            if agent_stem(path).is_some() && agent_mode(content).as_deref() == Some("primary") {
                text.push_str(content);
                text.push('\n');
            }
        }
        text
    }
}

fn load_one_file(
    root: &Path,
    path: &Path,
    files: &mut BTreeMap<String, String>,
) -> Result<(), String> {
    let metadata = std::fs::symlink_metadata(path)
        .map_err(|error| format!("cannot stat {}: {error}", path.display()))?;
    if metadata.file_type().is_symlink() {
        return Err(format!("harness tree contains a symlink: {}", path.display()));
    }
    if !metadata.is_file() {
        return Ok(());
    }
    if metadata.len() > MAX_TREE_FILE_BYTES as u64 {
        return Err(format!(
            "{} exceeds the {MAX_TREE_FILE_BYTES}-byte harness file limit",
            path.display()
        ));
    }
    let canonical = std::fs::canonicalize(path)
        .map_err(|error| format!("cannot resolve {}: {error}", path.display()))?;
    if !canonical.starts_with(root) {
        return Err(format!("{} escapes the harness root", path.display()));
    }
    let relative = canonical
        .strip_prefix(root)
        .map_err(|_| format!("{} is outside the harness root", path.display()))?;
    let relative = relative.to_string_lossy().replace('\\', "/");
    let mut bytes = Vec::new();
    std::fs::File::open(&canonical)
        .and_then(|file| file.take((MAX_TREE_FILE_BYTES + 1) as u64).read_to_end(&mut bytes))
        .map_err(|error| format!("cannot read {}: {error}", path.display()))?;
    let content =
        String::from_utf8(bytes).map_err(|_| format!("{} is not valid UTF-8", path.display()))?;
    files.insert(relative, content);
    Ok(())
}

fn collect_files(
    root: &Path,
    directory: &Path,
    depth: usize,
    files: &mut BTreeMap<String, String>,
) -> Result<(), String> {
    if depth > MAX_TREE_DEPTH {
        return Err(format!("harness tree exceeds depth {MAX_TREE_DEPTH}"));
    }
    let entries = std::fs::read_dir(directory)
        .map_err(|error| format!("cannot read {}: {error}", directory.display()))?;
    let mut paths = entries.filter_map(Result::ok).map(|entry| entry.path()).collect::<Vec<_>>();
    paths.sort();
    for path in paths {
        if files.len() >= MAX_TREE_FILES {
            return Err(format!("harness tree exceeds {MAX_TREE_FILES} files"));
        }
        let metadata = std::fs::symlink_metadata(&path)
            .map_err(|error| format!("cannot stat {}: {error}", path.display()))?;
        if metadata.file_type().is_symlink() {
            return Err(format!("harness tree contains a symlink: {}", path.display()));
        }
        if metadata.is_dir() {
            collect_files(root, &path, depth + 1, files)?;
            continue;
        }
        if !metadata.is_file() {
            continue;
        }
        if metadata.len() > MAX_TREE_FILE_BYTES as u64 {
            return Err(format!(
                "{} exceeds the {MAX_TREE_FILE_BYTES}-byte harness file limit",
                path.display()
            ));
        }
        let canonical = std::fs::canonicalize(&path)
            .map_err(|error| format!("cannot resolve {}: {error}", path.display()))?;
        if !canonical.starts_with(root) {
            return Err(format!("{} escapes the harness root", path.display()));
        }
        let relative = path
            .strip_prefix(root)
            .map_err(|_| format!("{} is outside the harness root", path.display()))?;
        let relative = relative.to_string_lossy().replace('\\', "/");
        let mut bytes = Vec::new();
        std::fs::File::open(&path)
            .and_then(|file| file.take((MAX_TREE_FILE_BYTES + 1) as u64).read_to_end(&mut bytes))
            .map_err(|error| format!("cannot read {}: {error}", path.display()))?;
        if bytes.len() > MAX_TREE_FILE_BYTES {
            return Err(format!(
                "{} exceeds the {MAX_TREE_FILE_BYTES}-byte harness file limit",
                path.display()
            ));
        }
        let content = String::from_utf8(bytes)
            .map_err(|_| format!("{} is not valid UTF-8", path.display()))?;
        files.insert(relative, content);
    }
    Ok(())
}

/// Which discard rule produced an error.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ValidationRule {
    /// The candidate tree has no files.
    EmptyTree,
    /// A file is outside the editable component surface.
    OffSurface,
    /// A component name is not a safe lowercase identifier.
    InvalidName,
    /// A component name shadows a reserved name.
    ReservedName,
    /// Two components share a name.
    DuplicateName,
    /// Frontmatter is missing, unparsable, or carries an invalid value.
    Frontmatter,
    /// Guidance names a script that the tree does not contain.
    ScriptReference,
    /// A script uses an extension outside the allowlist.
    ScriptAllowlist,
    /// A script failed its bounded `sh -n` / `py_compile` check.
    ScriptSyntax,
    /// Always-on text exceeds [`MAX_ALWAYS_ON_CHARS`].
    AlwaysOnCap,
}

/// One rejected rule with the file that triggered it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ValidationError {
    /// Tree-relative file, or a stable component label.
    pub file: String,
    /// Discard rule.
    pub rule: ValidationRule,
    /// Human-readable detail naming the fix.
    pub message: String,
}

/// One repair the validator applied to the returned tree.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Repair {
    /// File the repair changed.
    pub file: String,
    /// Rule the repair satisfied.
    pub rule: ValidationRule,
    /// What changed, without replaying the full file.
    pub detail: String,
}

/// Recorded component counts for the validated tree.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct HarnessCensus {
    /// Files in the tree.
    pub files: usize,
    /// Skill manifests.
    pub skills: usize,
    /// Allowed scripts.
    pub scripts: usize,
    /// Agent definitions.
    pub subagents: usize,
}

/// Result of validating one candidate tree.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Verdict {
    /// Whether the tree may be adopted after applying repairs.
    pub ok: bool,
    /// Tree with recorded repairs applied.
    pub tree: HarnessTree,
    /// Repairs applied, in application order.
    pub repairs_applied: Vec<Repair>,
    /// Discard-rule errors. `ok` is false when this is non-empty.
    pub errors: Vec<ValidationError>,
    /// Non-fatal notes, including repaired trigger lines.
    pub warnings: Vec<String>,
    /// Component counts.
    pub census: HarnessCensus,
    /// Always-on character count of the validated tree.
    pub always_on_chars: usize,
    /// Change in always-on characters against the optional base tree.
    pub always_on_char_delta: i64,
}

/// Validates one candidate tree, applying recorded trigger-line repairs.
///
/// `base` is the current harness tree when the candidate is a diff; its
/// always-on size feeds [`Verdict::always_on_char_delta`].
#[must_use]
pub fn validate(tree: &HarnessTree, base: Option<&HarnessTree>) -> Verdict {
    let mut verdict = Verdict {
        ok: false,
        tree: tree.clone(),
        repairs_applied: Vec::new(),
        errors: Vec::new(),
        warnings: Vec::new(),
        census: HarnessCensus::default(),
        always_on_chars: 0,
        always_on_char_delta: 0,
    };
    let mut working = tree.clone();
    if tree.is_empty() {
        verdict.errors.push(error(
            "(tree)",
            ValidationRule::EmptyTree,
            "harness tree is empty; edit the files, do not delete them all",
        ));
        return verdict;
    }
    validate_paths(&working, &mut verdict);
    validate_names(&working, &mut verdict);
    validate_frontmatter(&working, &mut verdict);
    validate_scripts(&working, &mut verdict);
    validate_script_references(&working, &mut verdict);
    repair_trigger_lines(&mut working, &mut verdict);
    validate_always_on(&working, base, &mut verdict);
    verdict.census = census(&working);
    verdict.tree = working;
    verdict.ok = verdict.errors.is_empty();
    verdict
}

fn error(file: &str, rule: ValidationRule, message: impl Into<String>) -> ValidationError {
    ValidationError { file: file.to_string(), rule, message: message.into() }
}

fn validate_paths(tree: &HarnessTree, verdict: &mut Verdict) {
    for path in tree.files.keys() {
        if !safe_relative_path(path) {
            verdict.errors.push(error(
                path,
                ValidationRule::OffSurface,
                "harness paths must be relative, dot-free, and stay inside the component root",
            ));
            continue;
        }
        let allowed = ROOT_FILES.contains(&path.as_str())
            || COMPONENT_DIRS.iter().any(|directory| {
                path.strip_prefix(directory)
                    .is_some_and(|rest| rest.starts_with('/') && rest.len() > 1)
            });
        if !allowed {
            verdict.errors.push(error(
                path,
                ValidationRule::OffSurface,
                format!(
                    "`{path}` is outside the editable harness surface; allowed entries are \
                     guardrails.md, tool_guidance.md, middleware.toml, and {COMPONENT_DIRS:?}"
                ),
            ));
        }
    }
}

fn safe_relative_path(path: &str) -> bool {
    if path.is_empty() || path.len() > 512 {
        return false;
    }
    let candidate = Path::new(path);
    !candidate.is_absolute()
        && candidate.components().all(|component| matches!(component, Component::Normal(_)))
        && candidate.components().count() <= MAX_TREE_DEPTH + 1
}

fn validate_names(tree: &HarnessTree, verdict: &mut Verdict) {
    let mut skills = BTreeSet::new();
    let mut agents = BTreeSet::new();
    let mut scripts = BTreeSet::new();
    let mut memories = BTreeSet::new();

    for (path, content) in &tree.files {
        if let Some(name) = skill_dir_name(path) {
            check_component_name(path, name, verdict);
            if RESERVED_COMPONENT_NAMES.contains(&name) {
                verdict.errors.push(error(
                    path,
                    ValidationRule::ReservedName,
                    format!("skill name `{name}` is reserved for another harness component"),
                ));
            }
            let declared = skill_declared_name(content).unwrap_or_else(|| name.to_string());
            if !skills.insert(declared.clone()) {
                verdict.errors.push(error(
                    path,
                    ValidationRule::DuplicateName,
                    format!("duplicate skill name `{declared}`"),
                ));
            }
        } else if let Some(name) = agent_stem(path) {
            check_component_name(path, name, verdict);
            let builtin = crate::subagent_roles::BuiltinSubagentRole::ALL
                .iter()
                .any(|role| role.name() == name);
            if builtin {
                verdict.errors.push(error(
                    path,
                    ValidationRule::ReservedName,
                    format!("agent name `{name}` is reserved for a builtin subagent role"),
                ));
            }
            if !agents.insert(name.to_string()) {
                verdict.errors.push(error(
                    path,
                    ValidationRule::DuplicateName,
                    format!("duplicate agent name `{name}`"),
                ));
            }
        } else if let Some((stem, extension)) = script_stem(path) {
            check_component_name(path, stem, verdict);
            if !SCRIPT_EXTENSIONS.contains(&extension) {
                verdict.errors.push(error(
                    path,
                    ValidationRule::ScriptAllowlist,
                    format!(
                        "script `{path}` must end in one of the allowed extensions {SCRIPT_EXTENSIONS:?}"
                    ),
                ));
            } else if !scripts.insert(stem.to_string()) {
                verdict.errors.push(error(
                    path,
                    ValidationRule::DuplicateName,
                    format!("duplicate script name `{stem}`"),
                ));
            }
        } else if let Some(stem) = memory_stem(path) {
            check_component_name(path, stem, verdict);
            if !memories.insert(stem.to_string()) {
                verdict.errors.push(error(
                    path,
                    ValidationRule::DuplicateName,
                    format!("duplicate memory name `{stem}`"),
                ));
            }
        }
    }
}

/// Component names that cannot be reused by a skill or script.
const RESERVED_COMPONENT_NAMES: &[&str] =
    &["guardrails", "tool_guidance", "middleware", "scripts", "skills", "agents", "memories"];

fn check_component_name(path: &str, name: &str, verdict: &mut Verdict) {
    let valid = !name.is_empty()
        && name.chars().count() <= MAX_COMPONENT_NAME_CHARS
        && name
            .chars()
            .next()
            .is_some_and(|character| character.is_ascii_lowercase() || character.is_ascii_digit())
        && name.chars().all(|character| {
            character.is_ascii_lowercase()
                || character.is_ascii_digit()
                || matches!(character, '_' | '-')
        });
    if !valid {
        verdict.errors.push(error(
            path,
            ValidationRule::InvalidName,
            format!(
                "component name `{name}` must be 1-{MAX_COMPONENT_NAME_CHARS} lowercase ascii letters, \
                 digits, '_' or '-' and start with a letter or digit"
            ),
        ));
    }
}

fn skill_dir_name(path: &str) -> Option<&str> {
    let rest = path.strip_prefix("skills/")?;
    let (name, tail) = rest.split_once('/')?;
    (tail == "SKILL.md").then_some(name)
}

fn agent_stem(path: &str) -> Option<&str> {
    let rest = path.strip_prefix("agents/")?;
    if rest.contains('/') {
        return None;
    }
    rest.strip_suffix(".md")
}

fn script_stem(path: &str) -> Option<(&str, &str)> {
    let rest = path.strip_prefix("scripts/")?;
    if rest.contains('/') {
        return None;
    }
    let (stem, extension) = rest.rsplit_once('.')?;
    Some((stem, extension))
}

fn memory_stem(path: &str) -> Option<&str> {
    let rest = path.strip_prefix("memories/")?;
    let file = rest.rsplit('/').next()?;
    file.strip_suffix(".md")
}

fn validate_frontmatter(tree: &HarnessTree, verdict: &mut Verdict) {
    for (path, content) in &tree.files {
        if path == "middleware.toml" {
            match toml::from_str::<crate::middleware::MiddlewareSpec>(content) {
                Ok(spec) => {
                    if let Err(message) = spec.validate() {
                        verdict.errors.push(error(path, ValidationRule::Frontmatter, message));
                    }
                }
                Err(parse_error) => {
                    verdict.errors.push(error(
                        path,
                        ValidationRule::Frontmatter,
                        format!("invalid middleware TOML: {parse_error}"),
                    ));
                }
            }
            continue;
        }
        let requires_frontmatter = skill_dir_name(path).is_some() || agent_stem(path).is_some();
        let starts_with_frontmatter = content.trim_start().starts_with("---");
        let frontmatter = markdown_config::split_frontmatter(content);
        let parsed = match frontmatter {
            Ok((yaml, _body)) => {
                match markdown_config::parse_frontmatter::<serde_json::Value>(yaml) {
                    Ok(value) => Some(value),
                    Err(message) => {
                        verdict.errors.push(error(path, ValidationRule::Frontmatter, message));
                        None
                    }
                }
            }
            Err(message) => {
                if requires_frontmatter || starts_with_frontmatter {
                    verdict.errors.push(error(
                        path,
                        ValidationRule::Frontmatter,
                        format!("missing or invalid frontmatter: {message}"),
                    ));
                }
                None
            }
        };
        let Some(value) = parsed else { continue };
        if !value.is_object() {
            verdict.errors.push(error(
                path,
                ValidationRule::Frontmatter,
                "frontmatter must be a YAML mapping",
            ));
            continue;
        }
        if skill_dir_name(path).is_some() && skill_declared_name(content).is_none() {
            verdict.errors.push(error(
                path,
                ValidationRule::Frontmatter,
                "skill frontmatter requires a non-empty `name` and `description`",
            ));
        }
        if let Some(name) = value.get("name")
            && !name.as_str().is_some_and(|name| !name.trim().is_empty())
        {
            verdict.errors.push(error(
                path,
                ValidationRule::Frontmatter,
                "`name` must be a non-empty string",
            ));
        }
        if let Some(mode) = value.get("mode")
            && !mode.as_str().is_some_and(|mode| matches!(mode, "primary" | "subagent"))
        {
            verdict.errors.push(error(
                path,
                ValidationRule::Frontmatter,
                "`mode` must be \"primary\" or \"subagent\"",
            ));
        }
        if let Some(steps) = value.get("steps")
            && !steps.as_u64().is_some_and(|steps| steps > 0)
        {
            verdict.errors.push(error(
                path,
                ValidationRule::Frontmatter,
                "`steps` must be a positive integer",
            ));
        }
    }
}

fn validate_scripts(tree: &HarnessTree, verdict: &mut Verdict) {
    for path in tree.files.keys() {
        if let Some((_, extension)) = script_stem(path)
            && !SCRIPT_EXTENSIONS.contains(&extension)
        {
            verdict.errors.push(error(
                path,
                ValidationRule::ScriptAllowlist,
                format!("script extension `.{extension}` is not allowed"),
            ));
        }
    }
}

fn validate_script_references(tree: &HarnessTree, verdict: &mut Verdict) {
    let mut sources = BTreeMap::new();
    for (path, content) in &tree.files {
        let is_prose_source = path == "guardrails.md"
            || path == "tool_guidance.md"
            || skill_dir_name(path).is_some()
            || agent_stem(path).is_some();
        if is_prose_source {
            sources.insert(path.clone(), content.clone());
        }
    }
    for (source, text) in sources {
        for reference in script_references(&text) {
            let exists = tree.files.contains_key(&reference);
            if !exists {
                verdict.errors.push(error(
                    &source,
                    ValidationRule::ScriptReference,
                    format!("references missing script `{reference}`; add the file or drop the reference"),
                ));
            }
        }
    }
}

/// Extracts normalized `scripts/<name>.<ext>` references from prose.
fn script_references(text: &str) -> BTreeSet<String> {
    let mut references = BTreeSet::new();
    let bytes = text.as_bytes();
    let needle = b"scripts/";
    let mut index = 0usize;
    while index + needle.len() <= bytes.len() {
        if &bytes[index..index + needle.len()] == needle {
            let start = index;
            let mut end = index + needle.len();
            while end < bytes.len()
                && !bytes[end].is_ascii_whitespace()
                && !matches!(bytes[end], b'`' | b')' | b'"' | b'\'' | b',' | b';' | b':')
            {
                end += 1;
            }
            let candidate = &text[start..end];
            if let Some(name) = candidate.strip_prefix("scripts/")
                && let Some((_, extension)) = name.rsplit_once('.')
                && SCRIPT_EXTENSIONS.contains(&extension)
            {
                references.insert(candidate.to_string());
            }
            index = end;
        } else {
            index += 1;
        }
    }
    references
}

fn repair_trigger_lines(tree: &mut HarnessTree, verdict: &mut Verdict) {
    let always_on = tree.always_on_text().to_ascii_lowercase();
    let mut repairs = Vec::new();
    let mut guardrails = tree.files.get("guardrails.md").cloned().unwrap_or_default();
    for (path, content) in &tree.files {
        let Some(skill) = skill_dir_name(path) else { continue };
        let name = skill_declared_name(content).unwrap_or_else(|| skill.to_string());
        if always_on.contains(&name.to_ascii_lowercase()) {
            continue;
        }
        let description = skill_description(content);
        if !guardrails.is_empty() && !guardrails.ends_with('\n') {
            guardrails.push('\n');
        }
        guardrails.push_str(&format!(
            "- Before working on \"{}\", load skill `{}`.\n",
            truncate_chars(&description, MAX_DESCRIPTION_CHARS),
            name
        ));
        repairs.push(Repair {
            file: "guardrails.md".to_string(),
            rule: ValidationRule::Frontmatter,
            detail: format!("autotrigger: named skill `{name}` in always-on text"),
        });
        verdict.warnings.push(format!(
            "skill `{name}` had no trigger line; autotrigger added one to guardrails.md"
        ));
    }
    if repairs.is_empty() {
        return;
    }
    tree.files.insert("guardrails.md".to_string(), guardrails);
    verdict.repairs_applied.extend(repairs);
}

fn validate_always_on(tree: &HarnessTree, base: Option<&HarnessTree>, verdict: &mut Verdict) {
    let chars = tree.always_on_text().chars().count();
    verdict.always_on_chars = chars;
    verdict.always_on_char_delta = match base {
        Some(base) => chars as i64 - base.always_on_text().chars().count() as i64,
        None => 0,
    };
    if chars > MAX_ALWAYS_ON_CHARS {
        verdict.errors.push(error(
            "guardrails.md",
            ValidationRule::AlwaysOnCap,
            format!(
                "always-on text is {chars} characters, over the {MAX_ALWAYS_ON_CHARS}-character \
                 budget; every always-on character competes with the output budget"
            ),
        ));
    }
}

fn census(tree: &HarnessTree) -> HarnessCensus {
    HarnessCensus {
        files: tree.files.len(),
        skills: tree.files.keys().filter(|path| skill_dir_name(path).is_some()).count(),
        scripts: tree
            .files
            .keys()
            .filter(|path| {
                script_stem(path)
                    .is_some_and(|(_, extension)| SCRIPT_EXTENSIONS.contains(&extension))
            })
            .count(),
        subagents: tree.files.keys().filter(|path| agent_stem(path).is_some()).count(),
    }
}

fn skill_declared_name(content: &str) -> Option<String> {
    frontmatter_value(content).and_then(|value| {
        value
            .get("name")
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|name| !name.is_empty())
            .map(str::to_string)
    })
}

fn skill_description(content: &str) -> String {
    frontmatter_value(content)
        .and_then(|value| {
            value
                .get("description")
                .and_then(serde_json::Value::as_str)
                .map(str::trim)
                .filter(|description| !description.is_empty())
                .map(str::to_string)
        })
        .unwrap_or_else(|| {
            content
                .lines()
                .map(str::trim)
                .find(|line| !line.is_empty() && *line != "---" && !line.contains(':'))
                .unwrap_or_default()
                .to_string()
        })
}

fn agent_mode(content: &str) -> Option<String> {
    frontmatter_value(content).and_then(|value| {
        value.get("mode").and_then(serde_json::Value::as_str).map(|mode| mode.trim().to_string())
    })
}

fn frontmatter_value(content: &str) -> Option<serde_json::Value> {
    let (yaml, _) = markdown_config::split_frontmatter(content).ok()?;
    markdown_config::parse_frontmatter::<serde_json::Value>(yaml).ok()
}

fn truncate_chars(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        return text.to_string();
    }
    let mut truncated: String = text.chars().take(max_chars).collect();
    truncated.push('…');
    truncated
}

/// Runs one bounded script syntax check.
///
/// Only supported extensions are accepted. The command is `sh -n <path>` or
/// `python3 -m py_compile <path>`; nothing from the file becomes an argument.
pub fn check_script_syntax(path: &Path, extension: &str) -> Result<(), String> {
    let mut command = match extension {
        "sh" => {
            let mut command = Command::new("sh");
            command.arg("-n").arg(path);
            command
        }
        "py" => {
            let mut command = Command::new("python3");
            command.arg("-m").arg("py_compile").arg(path);
            command
        }
        other => {
            return Err(format!(
                "script extension `.{other}` is not allowed; expected one of {SCRIPT_EXTENSIONS:?}"
            ));
        }
    };
    command.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = command
        .spawn()
        .map_err(|error| format!("cannot start syntax check for {}: {error}", path.display()))?;
    let status = wait_bounded(&mut child)?;
    let output = child
        .wait_with_output()
        .map_err(|error| format!("cannot collect syntax check output: {error}"))?;
    if status {
        return Ok(());
    }
    let mut detail = String::from_utf8_lossy(&output.stderr).to_string();
    if detail.trim().is_empty() {
        detail = String::from_utf8_lossy(&output.stdout).to_string();
    }
    Err(format!(
        "{} failed its syntax check: {}",
        path.display(),
        truncate_chars(detail.trim(), SYNTAX_CHECK_OUTPUT_BYTES)
    ))
}

fn wait_bounded(child: &mut Child) -> Result<bool, String> {
    let deadline = Instant::now() + SYNTAX_CHECK_TIMEOUT;
    loop {
        if let Some(status) =
            child.try_wait().map_err(|error| format!("syntax check failed: {error}"))?
        {
            return Ok(status.success());
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return Err(format!("syntax check exceeded {SYNTAX_CHECK_TIMEOUT:?}"));
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// Materializes a tree under `root` for driver-side syntax checks.
///
/// Refuses paths outside the tree and files larger than the tree cap.
pub fn materialize(tree: &HarnessTree, root: &Path) -> Result<Vec<PathBuf>, String> {
    let mut written = Vec::new();
    for (relative, content) in &tree.files {
        if !safe_relative_path(relative) {
            return Err(format!("refusing to materialize unsafe path `{relative}`"));
        }
        let path = root.join(relative);
        if content.len() > MAX_TREE_FILE_BYTES {
            return Err(format!("refusing to materialize oversized file `{relative}`"));
        }
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|error| format!("cannot create {}: {error}", parent.display()))?;
        }
        std::fs::write(&path, content)
            .map_err(|error| format!("cannot write {}: {error}", path.display()))?;
        written.push(path);
    }
    Ok(written)
}

#[cfg(test)]
mod tests;
