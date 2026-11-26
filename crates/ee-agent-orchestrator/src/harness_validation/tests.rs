use super::*;

fn tree(files: &[(&str, &str)]) -> HarnessTree {
    HarnessTree::new(
        files.iter().map(|(path, content)| ((*path).to_string(), (*content).to_string())).collect(),
    )
}

fn skill(name: &str, body: &str) -> String {
    format!("---\nname: {name}\ndescription: {name} helper\n---\n{body}\n")
}

fn agent(name: &str, mode: &str) -> String {
    format!("---\nmode: {mode}\n---\nAct as {name}.\n")
}

#[test]
fn valid_triggered_tree_passes_with_census() {
    let candidate = tree(&[
        ("guardrails.md", "Never force-push. Load skill atlas before navigating code.\n"),
        ("tool_guidance.md", "Use scripts/check.sh for validation.\n"),
        ("scripts/check.sh", "#!/bin/sh\nexit 0\n"),
        ("skills/atlas/SKILL.md", &skill("atlas", "Use the graph first.")),
        ("agents/helper.md", &agent("helper", "subagent")),
        ("memories/rules.md", "Run tests with --quiet.\n"),
    ]);
    let verdict = validate(&candidate, None);
    assert!(verdict.ok, "{:?}", verdict.errors);
    assert!(verdict.repairs_applied.is_empty());
    assert_eq!(verdict.census.files, 6);
    assert_eq!(verdict.census.skills, 1);
    assert_eq!(verdict.census.scripts, 1);
    assert_eq!(verdict.census.subagents, 1);
}

#[test]
fn missing_trigger_line_is_repaired_and_recorded() {
    let candidate = tree(&[
        ("guardrails.md", "Never force-push.\n"),
        ("skills/atlas/SKILL.md", &skill("atlas", "Use the graph first.")),
    ]);
    let base = tree(&[("guardrails.md", "Never force-push.\n")]);
    let verdict = validate(&candidate, Some(&base));
    assert!(verdict.ok, "{:?}", verdict.errors);
    assert_eq!(verdict.repairs_applied.len(), 1);
    assert!(verdict.repairs_applied[0].detail.contains("atlas"));
    let guardrails = verdict.tree.files.get("guardrails.md").expect("guardrails");
    assert!(guardrails.contains("load skill `atlas`"));
    assert!(verdict.always_on_char_delta > 0, "the repair grows always-on text");
}

#[test]
fn missing_script_reference_is_rejected_by_name() {
    let candidate = tree(&[("guardrails.md", "Always run scripts/missing.sh first.\n")]);
    let verdict = validate(&candidate, None);
    assert!(!verdict.ok);
    let error = verdict
        .errors
        .iter()
        .find(|error| error.rule == ValidationRule::ScriptReference)
        .expect("script reference error");
    assert_eq!(error.file, "guardrails.md");
    assert!(error.message.contains("scripts/missing.sh"));
}

#[test]
fn workspace_script_reference_is_allowed_when_declared_external() {
    let candidate = tree(&[("guardrails.md", "Run scripts/run-affected-tests.sh first.\n")]);
    let external = BTreeSet::from([String::from("scripts/run-affected-tests.sh")]);
    let verdict = validate_with_external_scripts(&candidate, None, &external);
    assert!(verdict.ok, "{:?}", verdict.errors);
}

#[test]
fn invalid_frontmatter_is_rejected() {
    let candidate = tree(&[("skills/broken/SKILL.md", "---\nname: [unterminated\n---\nbody\n")]);
    let verdict = validate(&candidate, None);
    assert!(!verdict.ok);
    assert!(
        verdict.errors.iter().any(|error| error.rule == ValidationRule::Frontmatter
            && error.file == "skills/broken/SKILL.md"),
        "{:?}",
        verdict.errors
    );
}

#[test]
fn off_surface_file_is_named_in_the_error() {
    let candidate = tree(&[("guardrails.md", "Rule.\n"), ("notes.txt", "hi\n")]);
    let verdict = validate(&candidate, None);
    assert!(!verdict.ok);
    let error = verdict
        .errors
        .iter()
        .find(|error| error.rule == ValidationRule::OffSurface)
        .expect("off-surface error");
    assert_eq!(error.file, "notes.txt");
}

#[test]
fn invalid_and_reserved_names_are_rejected() {
    let candidate = tree(&[
        ("skills/Bad Name/SKILL.md", &skill("bad", "body")),
        ("agents/reviewer.md", &agent("reviewer", "subagent")),
    ]);
    let verdict = validate(&candidate, None);
    assert!(!verdict.ok);
    assert!(
        verdict.errors.iter().any(|error| error.rule == ValidationRule::InvalidName),
        "{:?}",
        verdict.errors
    );
    assert!(
        verdict.errors.iter().any(|error| error.rule == ValidationRule::ReservedName
            && error.file == "agents/reviewer.md"),
        "{:?}",
        verdict.errors
    );
}

#[test]
fn duplicate_skill_names_are_rejected() {
    let candidate = tree(&[
        ("skills/one/SKILL.md", &skill("same", "one")),
        ("skills/two/SKILL.md", &skill("same", "two")),
    ]);
    let verdict = validate(&candidate, None);
    assert!(!verdict.ok);
    assert!(
        verdict.errors.iter().any(|error| error.rule == ValidationRule::DuplicateName),
        "{:?}",
        verdict.errors
    );
}

#[test]
fn empty_tree_is_rejected() {
    let verdict = validate(&HarnessTree::default(), None);
    assert!(!verdict.ok);
    assert_eq!(verdict.errors[0].rule, ValidationRule::EmptyTree);
}

#[test]
fn always_on_cap_is_enforced_with_delta() {
    let candidate = tree(&[("guardrails.md", &"x".repeat(MAX_ALWAYS_ON_CHARS + 1))]);
    let base = tree(&[("guardrails.md", "short\n")]);
    let verdict = validate(&candidate, Some(&base));
    assert!(!verdict.ok);
    assert!(
        verdict.errors.iter().any(|error| error.rule == ValidationRule::AlwaysOnCap),
        "{:?}",
        verdict.errors
    );
    assert!(verdict.always_on_char_delta > 0);
}

#[test]
fn script_extension_allowlist_is_enforced() {
    let candidate = tree(&[("scripts/tool.rs", "fn main() {}\n")]);
    let verdict = validate(&candidate, None);
    assert!(!verdict.ok);
    assert!(
        verdict.errors.iter().any(|error| error.rule == ValidationRule::ScriptAllowlist),
        "{:?}",
        verdict.errors
    );
}

#[test]
fn middleware_frontmatter_reuses_the_strict_parser() {
    let candidate = tree(&[("middleware.toml", "unknown_hook = true\n")]);
    let verdict = validate(&candidate, None);
    assert!(!verdict.ok);
    assert!(
        verdict
            .errors
            .iter()
            .any(|error| error.rule == ValidationRule::Frontmatter
                && error.file == "middleware.toml"),
        "{:?}",
        verdict.errors
    );
}

#[test]
fn sh_syntax_check_accepts_and_rejects() {
    let temp = tempfile::tempdir().expect("tempdir");
    let good = temp.path().join("good.sh");
    std::fs::write(&good, "#!/bin/sh\nif true; then\n  exit 0\nfi\n").expect("write");
    assert!(check_script_syntax(&good, "sh").is_ok());

    let bad = temp.path().join("bad.sh");
    std::fs::write(&bad, "if then\n").expect("write");
    let error = check_script_syntax(&bad, "sh").expect_err("invalid shell");
    assert!(error.contains("syntax check"), "{error}");
}

#[test]
fn syntax_check_rejects_unknown_extension_and_missing_file() {
    let temp = tempfile::tempdir().expect("tempdir");
    let file = temp.path().join("run.rs");
    std::fs::write(&file, "fn main() {}\n").expect("write");
    assert!(check_script_syntax(&file, "rs").expect_err("allowlist").contains("not allowed"));
    assert!(check_script_syntax(&temp.path().join("missing.sh"), "sh").is_err());
}

#[test]
fn tree_load_is_bounded_and_rejects_symlinks() {
    let temp = tempfile::tempdir().expect("tempdir");
    std::fs::write(temp.path().join("guardrails.md"), "Rule.\n").expect("write");
    let tree = HarnessTree::load(temp.path()).expect("loads");
    assert!(tree.files.contains_key("guardrails.md"));

    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(temp.path().join("guardrails.md"), temp.path().join("link.md"))
            .expect("symlink");
        let error = HarnessTree::load(temp.path()).expect_err("symlink rejected");
        assert!(error.contains("symlink"), "{error}");
    }
}

#[test]
fn materialize_writes_safe_paths_only() {
    let temp = tempfile::tempdir().expect("tempdir");
    let good = tree(&[("scripts/a.sh", "#!/bin/sh\n")]);
    let written = materialize(&good, temp.path()).expect("materializes");
    assert_eq!(written.len(), 1);
    assert!(temp.path().join("scripts/a.sh").is_file());

    let bad = tree(&[("../escape.sh", "#!/bin/sh\n")]);
    assert!(materialize(&bad, temp.path()).is_err());
}
