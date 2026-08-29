//! Versioned, application-owned non-overridable safety checks.
//!
//! Safeguards intentionally cover only catastrophic operations. They inspect
//! typed paths and a bounded POSIX-shell subset; no raw substring grants or
//! denies authority.

use std::path::{Component, Path, PathBuf};

use super::command::SHELL_WRAPPERS;

pub(crate) const SAFEGUARD_REGISTRY_VERSION: u64 = 1;

pub(crate) const CATASTROPHIC_DELETE_RULE_ID: &str = "builtin.v1.catastrophic-recursive-delete";
pub(crate) const PROTECTED_STATE_RULE_ID: &str = "builtin.v1.protected-state-mutation";
pub(crate) const SPECIAL_FILE_RULE_ID: &str = "builtin.v1.special-file-mutation";
pub(crate) const PATH_ESCAPE_RULE_ID: &str = "builtin.v1.canonical-path-escape";
pub(crate) const SHELL_PARSE_RULE_ID: &str = "builtin.v1.guarded-shell-parse-failure";

const MAX_SCRIPT_BYTES: usize = 16 * 1024;
const MAX_COMPONENTS: usize = 128;
const MAX_TOKENS: usize = 256;
const MAX_NESTING: usize = 8;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SafeguardCategory {
    CatastrophicDeletion,
    ProtectedStateMutation,
    SpecialFileMutation,
    CanonicalPathEscape,
    GuardedShellParseFailure,
}

impl SafeguardCategory {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::CatastrophicDeletion => "catastrophic_deletion",
            Self::ProtectedStateMutation => "protected_state_mutation",
            Self::SpecialFileMutation => "special_file_mutation",
            Self::CanonicalPathEscape => "canonical_path_escape",
            Self::GuardedShellParseFailure => "guarded_shell_parse_failure",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct SafeguardMatch {
    pub(crate) rule_id: &'static str,
    pub(crate) category: SafeguardCategory,
}

impl SafeguardMatch {
    pub(crate) const fn new(rule_id: &'static str, category: SafeguardCategory) -> Self {
        Self { rule_id, category }
    }
}

/// Classifies one shell-backed terminal request. Explicit `args` are literals
/// appended by ee after shell quoting and therefore join the final simple
/// command only.
pub(crate) fn inspect_terminal_command(
    command: &str,
    args: &[String],
    cwd: &Path,
    workspace_roots: &[PathBuf],
    home: Option<&Path>,
    protected_files: &[PathBuf],
) -> Option<SafeguardMatch> {
    match parse_script(command, 0) {
        Ok(mut commands) => {
            if let Some(last) = commands.last_mut() {
                last.extend(args.iter().cloned());
            }
            inspect_commands(&commands, cwd, workspace_roots, home, protected_files, 0)
        }
        Err(error) if error.guarded => Some(SafeguardMatch::new(
            SHELL_PARSE_RULE_ID,
            SafeguardCategory::GuardedShellParseFailure,
        )),
        Err(_) => None,
    }
}

fn inspect_commands(
    commands: &[Vec<String>],
    cwd: &Path,
    workspace_roots: &[PathBuf],
    home: Option<&Path>,
    protected_files: &[PathBuf],
    depth: usize,
) -> Option<SafeguardMatch> {
    if depth > MAX_NESTING {
        return Some(SafeguardMatch::new(
            SHELL_PARSE_RULE_ID,
            SafeguardCategory::GuardedShellParseFailure,
        ));
    }
    for command in commands {
        let Some((executable, argv)) = split_executable(command) else {
            continue;
        };
        let basename =
            Path::new(executable).file_name().and_then(|name| name.to_str()).unwrap_or(executable);
        let normalized_basename = basename.to_ascii_lowercase();
        if let Some(matched) = inspect_command_mutation_paths(
            &normalized_basename,
            argv,
            cwd,
            workspace_roots,
            protected_files,
        ) {
            return Some(matched);
        }
        if is_recursive_delete(&normalized_basename, argv)
            && delete_targets(argv)
                .iter()
                .any(|target| is_catastrophic_target(target, cwd, workspace_roots, home))
        {
            return Some(SafeguardMatch::new(
                CATASTROPHIC_DELETE_RULE_ID,
                SafeguardCategory::CatastrophicDeletion,
            ));
        }
        if SHELL_WRAPPERS.contains(&normalized_basename.as_str())
            && let Some(nested_result) =
                nested_shell_commands(&normalized_basename, argv, depth + 1)
        {
            match nested_result {
                Ok(nested) => {
                    if let Some(matched) = inspect_commands(
                        &nested,
                        cwd,
                        workspace_roots,
                        home,
                        protected_files,
                        depth + 1,
                    ) {
                        return Some(matched);
                    }
                }
                Err(_) => {
                    return Some(SafeguardMatch::new(
                        SHELL_PARSE_RULE_ID,
                        SafeguardCategory::GuardedShellParseFailure,
                    ));
                }
            }
        }
    }
    None
}

fn split_executable(command: &[String]) -> Option<(&str, &[String])> {
    let index = command.iter().position(|token| !is_assignment(token))?;
    Some((&command[index], &command[index + 1..]))
}

fn is_assignment(token: &str) -> bool {
    let Some((name, _)) = token.split_once('=') else {
        return false;
    };
    !name.is_empty()
        && name.chars().enumerate().all(|(index, character)| {
            character == '_'
                || character.is_ascii_alphanumeric() && (index > 0 || !character.is_ascii_digit())
        })
}

fn inspect_command_mutation_paths(
    executable: &str,
    argv: &[String],
    cwd: &Path,
    workspace_roots: &[PathBuf],
    protected_files: &[PathBuf],
) -> Option<SafeguardMatch> {
    let mut targets = Vec::new();
    for pair in argv.windows(2) {
        if matches!(pair[0].as_str(), ">" | ">>") {
            targets.push(pair[1].as_str());
        }
    }
    if executable == "tee" {
        targets.extend(argv.iter().map(String::as_str).filter(|arg| !arg.starts_with('-')));
    }
    if executable == "dd" {
        targets.extend(argv.iter().filter_map(|arg| arg.strip_prefix("of=")));
    }
    targets.into_iter().find_map(|target| {
        let path = lexical_absolute(Path::new(target), cwd);
        inspect_protected_state_path(&path, protected_files)
            .or_else(|| inspect_special_file(&path))
            .or_else(|| inspect_path_escape(&path, workspace_roots))
    })
}

fn is_recursive_delete(executable: &str, argv: &[String]) -> bool {
    match executable {
        "rm" => argv.iter().take_while(|arg| arg.as_str() != "--").any(|arg| {
            arg == "--recursive"
                || arg
                    .strip_prefix('-')
                    .is_some_and(|flags| flags.contains('r') || flags.contains('R'))
        }),
        "rmdir" | "rd" => argv.iter().any(|arg| arg.eq_ignore_ascii_case("/s")),
        "remove-item" => argv.iter().any(|arg| arg.eq_ignore_ascii_case("-recurse")),
        _ => false,
    }
}

fn delete_targets(argv: &[String]) -> Vec<&str> {
    let mut options = true;
    argv.iter()
        .filter_map(|arg| {
            if options && arg == "--" {
                options = false;
                return None;
            }
            if options && arg.starts_with('-') {
                return None;
            }
            if options && arg.eq_ignore_ascii_case("/s") {
                return None;
            }
            Some(arg.as_str())
        })
        .collect()
}

fn is_catastrophic_target(
    target: &str,
    cwd: &Path,
    workspace_roots: &[PathBuf],
    home: Option<&Path>,
) -> bool {
    let (base, wildcard_all) = wildcard_base(target);
    let expanded = expand_home(base, home);
    let resolved = lexical_absolute(&expanded, cwd);
    let cwd = lexical_normalize(cwd);
    let parent = cwd.parent().map(lexical_normalize);
    let home = home.map(lexical_normalize);
    let roots: Vec<PathBuf> = workspace_roots.iter().map(|root| lexical_normalize(root)).collect();
    let protected = resolved.parent().is_none()
        || resolved == cwd
        || parent.as_ref().is_some_and(|path| resolved == *path)
        || home.as_ref().is_some_and(|path| resolved == *path)
        || roots.contains(&resolved);
    protected || wildcard_all && (base.is_empty() || base == ".")
}

fn wildcard_base(target: &str) -> (&str, bool) {
    match target {
        "*" | "?" | "{*,.*}" => (".", true),
        _ => target
            .strip_suffix("/*")
            .or_else(|| target.strip_suffix("/?"))
            .map_or((target, false), |base| (base, true)),
    }
}

fn expand_home(target: &str, home: Option<&Path>) -> PathBuf {
    match (target, home) {
        ("~", Some(home)) => home.to_path_buf(),
        (_, Some(home)) if target.starts_with("~/") => home.join(&target[2..]),
        _ => PathBuf::from(target),
    }
}

fn lexical_absolute(path: &Path, cwd: &Path) -> PathBuf {
    if path.is_absolute() { lexical_normalize(path) } else { lexical_normalize(&cwd.join(path)) }
}

fn lexical_normalize(path: &Path) -> PathBuf {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                normalized.pop();
            }
            other => normalized.push(other.as_os_str()),
        }
    }
    normalized
}

fn nested_shell_commands(
    shell: &str,
    argv: &[String],
    depth: usize,
) -> Option<Result<Vec<Vec<String>>, ParseError>> {
    if matches!(shell, "cmd" | "powershell" | "pwsh") {
        let marker = argv.iter().position(|arg| {
            arg.eq_ignore_ascii_case("/c") || arg.eq_ignore_ascii_case("-command")
        })?;
        let nested = argv.get(marker + 1..)?;
        if nested.is_empty() {
            return Some(Err(ParseError { guarded: true }));
        }
        return Some(Ok(vec![nested.to_vec()]));
    }
    argv.windows(2).find_map(|pair| {
        matches!(pair[0].as_str(), "-c" | "-lc").then(|| parse_script(&pair[1], depth))
    })
}

/// Exact application-owned state-path check. Parent directories are not denied:
/// only vault/store files and their atomic temporary siblings are protected.
pub(crate) fn inspect_protected_state_path(
    path: &Path,
    protected_files: &[PathBuf],
) -> Option<SafeguardMatch> {
    let path = canonical_or_lexical(path);
    protected_files.iter().find_map(|protected| {
        let protected = canonical_or_lexical(protected);
        let exact = path == protected;
        let temp_sibling = path.parent() == protected.parent()
            && path.file_name().is_some_and(|name| {
                let name = name.to_string_lossy();
                protected.file_name().is_some_and(|protected_name| {
                    name.starts_with(&format!(".{}.", protected_name.to_string_lossy()))
                })
            });
        (exact || temp_sibling).then_some(SafeguardMatch::new(
            PROTECTED_STATE_RULE_ID,
            SafeguardCategory::ProtectedStateMutation,
        ))
    })
}

pub(crate) fn inspect_special_file(path: &Path) -> Option<SafeguardMatch> {
    let metadata = std::fs::symlink_metadata(path).ok()?;
    let kind = metadata.file_type();
    (!kind.is_file() && !kind.is_dir() && !kind.is_symlink()).then_some(SafeguardMatch::new(
        SPECIAL_FILE_RULE_ID,
        SafeguardCategory::SpecialFileMutation,
    ))
}

pub(crate) fn inspect_path_escape(path: &Path, roots: &[PathBuf]) -> Option<SafeguardMatch> {
    let resolved = canonical_or_lexical(path);
    (!roots.iter().any(|root| resolved.starts_with(canonical_or_lexical(root))))
        .then_some(SafeguardMatch::new(PATH_ESCAPE_RULE_ID, SafeguardCategory::CanonicalPathEscape))
}

fn canonical_or_lexical(path: &Path) -> PathBuf {
    if let Ok(canonical) = std::fs::canonicalize(path) {
        return canonical;
    }
    let mut ancestor = path;
    let mut suffix = Vec::new();
    while !ancestor.exists() {
        let Some(name) = ancestor.file_name() else {
            return lexical_normalize(path);
        };
        suffix.push(name.to_os_string());
        let Some(parent) = ancestor.parent() else {
            return lexical_normalize(path);
        };
        ancestor = parent;
    }
    let mut resolved =
        std::fs::canonicalize(ancestor).unwrap_or_else(|_| lexical_normalize(ancestor));
    for component in suffix.iter().rev() {
        resolved.push(component);
    }
    lexical_normalize(&resolved)
}

#[derive(Debug)]
struct ParseError {
    guarded: bool,
}

fn parse_script(script: &str, depth: usize) -> Result<Vec<Vec<String>>, ParseError> {
    if depth > MAX_NESTING || script.len() > MAX_SCRIPT_BYTES {
        return Err(ParseError { guarded: true });
    }
    let mut commands = Vec::new();
    let mut command = Vec::new();
    let mut word = String::new();
    let mut quote = None;
    let mut escaped = false;
    let mut chars = script.chars().peekable();
    let mut guarded = false;

    while let Some(character) = chars.next() {
        if escaped {
            word.push(character);
            escaped = false;
            continue;
        }
        match quote {
            Some('\'') => {
                if character == '\'' {
                    quote = None;
                } else {
                    word.push(character);
                }
                continue;
            }
            Some('"') => match character {
                '"' => quote = None,
                '\\' => escaped = true,
                '$' | '`' => {
                    return Err(ParseError {
                        guarded: guarded || command_is_guarded(&command, &word),
                    });
                }
                _ => word.push(character),
            },
            _ => {}
        }
        if quote.is_some() {
            continue;
        }
        match character {
            '\'' | '"' => quote = Some(character),
            '\\' => escaped = true,
            '$' | '`' | '<' => {
                return Err(ParseError { guarded: guarded || command_is_guarded(&command, &word) });
            }
            '>' => {
                finish_word(&mut command, &mut word);
                let operator = if chars.peek() == Some(&'>') {
                    chars.next();
                    ">>"
                } else {
                    ">"
                };
                command.push(operator.to_string());
            }
            '&' | '|' => {
                let expected = character;
                if chars.peek() == Some(&expected) {
                    chars.next();
                } else if character == '&' {
                    return Err(ParseError {
                        guarded: guarded || command_is_guarded(&command, &word),
                    });
                }
                finish_word(&mut command, &mut word);
                guarded |= command_guarded(&command);
                finish_command(&mut commands, &mut command)?;
            }
            ';' | '(' | ')' | '\n' => {
                finish_word(&mut command, &mut word);
                guarded |= command_guarded(&command);
                finish_command(&mut commands, &mut command)?;
            }
            character if character.is_whitespace() => finish_word(&mut command, &mut word),
            '*' | '?' | '{' | '}' => word.push(character),
            _ => word.push(character),
        }
        if commands.len() > MAX_COMPONENTS || command.len() > MAX_TOKENS {
            return Err(ParseError { guarded: true });
        }
    }
    if quote.is_some() || escaped {
        return Err(ParseError { guarded: guarded || command_is_guarded(&command, &word) });
    }
    finish_word(&mut command, &mut word);
    finish_command(&mut commands, &mut command)?;
    if commands.is_empty() {
        return Err(ParseError { guarded: false });
    }
    Ok(commands)
}

fn finish_word(command: &mut Vec<String>, word: &mut String) {
    if !word.is_empty() {
        command.push(std::mem::take(word));
    }
}

fn finish_command(
    commands: &mut Vec<Vec<String>>,
    command: &mut Vec<String>,
) -> Result<(), ParseError> {
    if !command.is_empty() {
        commands.push(std::mem::take(command));
    }
    if commands.len() > MAX_COMPONENTS {
        return Err(ParseError { guarded: true });
    }
    Ok(())
}

fn command_is_guarded(command: &[String], word: &str) -> bool {
    let mut candidate = command.to_vec();
    if !word.is_empty() {
        candidate.push(word.to_string());
    }
    command_guarded(&candidate)
}

fn command_guarded(command: &[String]) -> bool {
    split_executable(command).is_some_and(|(executable, _)| {
        let basename =
            Path::new(executable).file_name().and_then(|name| name.to_str()).unwrap_or(executable);
        let basename = basename.to_ascii_lowercase();
        matches!(basename.as_str(), "rm" | "rmdir" | "rd" | "remove-item")
            || SHELL_WRAPPERS.contains(&basename.as_str())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn inspect(script: &str) -> Option<SafeguardMatch> {
        inspect_terminal_command(
            script,
            &[],
            Path::new("/work/project"),
            &[PathBuf::from("/work/project")],
            Some(Path::new("/home/user")),
            &[],
        )
    }

    #[test]
    fn decomposes_chains_pipelines_groups_and_nested_shells() {
        for script in [
            "echo ok && rm -rf /",
            "echo ok || ( rm -R . )",
            "printf x | rm --recursive -- /home/user",
            "sh -c 'echo ok; rm -rf /work/project'",
            "cmd /c rd /s /",
            "REMOVE-ITEM -Recurse /",
        ] {
            assert_eq!(
                inspect(script).map(|matched| matched.category),
                Some(SafeguardCategory::CatastrophicDeletion),
                "script: {script}"
            );
        }
    }

    #[test]
    fn quoting_whitespace_flags_and_wildcards_are_structural() {
        for script in [
            "rm   -fR   '/'",
            "rm --force --recursive .",
            "rm -rf -- *",
            "rm -r ~/",
            "rm -r /work/project/*",
        ] {
            assert!(inspect(script).is_some(), "script: {script}");
        }
        for script in [
            "echo 'rm -rf /'",
            "rm file-named-rf",
            "rm -r /work/project-copy",
            "remove-all /",
            "echo /work/project/*",
        ] {
            assert_eq!(inspect(script), None, "script: {script}");
        }

        assert_eq!(
            inspect_terminal_command(
                "rm -rf /srv",
                &[],
                Path::new("/srv"),
                &[PathBuf::from("/srv")],
                None,
                &[],
            )
            .map(|matched| matched.category),
            Some(SafeguardCategory::CatastrophicDeletion)
        );
    }

    #[test]
    fn explicit_args_join_final_compound_command_without_forcing_denial() {
        assert_eq!(
            inspect_terminal_command(
                "echo ok; printf done",
                &["argument".to_string()],
                Path::new("/work/project"),
                &[PathBuf::from("/work/project")],
                Some(Path::new("/home/user")),
                &[],
            ),
            None
        );
    }

    #[test]
    fn expansion_or_ambiguity_denies_only_after_guarded_command_entry() {
        assert_eq!(
            inspect("rm -rf \"$TARGET\"").map(|matched| matched.category),
            Some(SafeguardCategory::GuardedShellParseFailure)
        );
        assert_eq!(inspect("echo \"$TARGET\""), None);
        assert_eq!(inspect("echo rm -rf /"), None);
    }

    #[test]
    fn protected_state_matching_is_exact_and_ignores_similar_names() {
        assert_eq!(SAFEGUARD_REGISTRY_VERSION, 1);
        let protected = PathBuf::from("/work/project/state/trust.toml");
        assert_eq!(
            inspect_protected_state_path(&protected, std::slice::from_ref(&protected))
                .map(|matched| matched.category),
            Some(SafeguardCategory::ProtectedStateMutation)
        );
        assert_eq!(
            inspect_protected_state_path(
                Path::new("/work/project/state/trust.toml.backup"),
                &[protected]
            ),
            None
        );
    }

    #[cfg(unix)]
    #[test]
    fn output_redirections_and_device_writer_arguments_are_typed_paths() {
        for script in ["echo x > /dev/null", "tee /dev/null", "dd if=/tmp/in of=/dev/null"] {
            assert_eq!(
                inspect(script).map(|matched| matched.category),
                Some(SafeguardCategory::SpecialFileMutation),
                "script: {script}"
            );
        }
        assert_eq!(inspect("echo x > build.log"), None);
    }
}
