use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use super::*;

fn entry_file(
    seq: u64,
    domain: &str,
    task: &str,
    verdict: &str,
    experience: Option<(&str, &str, &str)>,
) -> String {
    let mut text = format!(
        "---\nseq: {seq}\ndomain: {domain}\ntask: {task}\nconfidence: medium\n---\n\
         ## Edits\ncomponent: tool_guidance.md\nchange: added rule\nobserved: candidate did X (run c2, step 11)\nverdict: {verdict}\n"
    );
    text.push_str("## Experience\n");
    match experience {
        Some((cue, lesson, evidence)) => {
            text.push_str(&format!("cue: {cue}\nlesson: {lesson}\nevidence: {evidence}\n"));
        }
        None => text.push_str("NO_ENTRY\n"),
    }
    text
}

fn write_entry(root: &Path, domain: &str, dir: &str, content: &str) -> PathBuf {
    let path =
        root.join(WORKSPACE_EXPERIENCE_DIR).join(domain).join("entries").join(dir).join(ENTRY_FILE);
    std::fs::create_dir_all(path.parent().expect("parent")).expect("create dirs");
    std::fs::write(&path, content).expect("write entry");
    path
}

fn sample_entry(seq: u64, domain: &str, task: &str) -> ExperienceEntry {
    ExperienceEntry {
        seq,
        domain: domain.to_string(),
        task: task.to_string(),
        confidence: ExperienceConfidence::Medium,
        edits: vec![EditRow {
            component: "tool_guidance.md".to_string(),
            candidates: vec!["c2".to_string()],
            change: "added reopen-and-check rule".to_string(),
            observed: "candidate re-opened outputs; base did not (run c2, step 11)".to_string(),
            verdict: EditVerdict::Helped,
        }],
        cue: Some("deliverables written but never reopened".to_string()),
        lesson: Some("add a verify-after-write guardrail".to_string()),
        evidence: Some("contrast between run c2 and base b1".to_string()),
        content: String::new(),
        scope: ExperienceScope::Workspace,
        path: PathBuf::new(),
        display_path: String::new(),
    }
}

#[test]
fn discovers_entries_and_loads_them_by_domain() {
    let workspace = tempfile::tempdir().expect("workspace");
    write_entry(
        workspace.path(),
        "rust-crates",
        "1-first-task",
        &entry_file(
            1,
            "rust-crates",
            "first task",
            "helped",
            Some(("cue text", "lesson text", "evidence text")),
        ),
    );

    let registry = ExperienceRegistry::discover(workspace.path(), &[], None);
    assert_eq!(registry.len(), 1);
    let entry = registry.iter().next().expect("entry");
    assert_eq!(entry.domain, "rust-crates");
    assert_eq!(entry.task, "first task");
    assert_eq!(entry.cue.as_deref(), Some("cue text"));
    assert_eq!(entry.edits[0].verdict, EditVerdict::Helped);
    assert_eq!(registry.next_seq("rust-crates"), 2);

    let loaded = load_experience("rust-crates", None, workspace.path(), &[], None).expect("load");
    assert!(loaded.text.contains("component: tool_guidance.md"));
    assert!(!loaded.truncated);
    let loaded_by_seq =
        load_experience("rust-crates", Some(1), workspace.path(), &[], None).expect("load seq");
    assert_eq!(loaded_by_seq.entry.seq, 1);
}

#[test]
fn no_entry_is_valid_and_retained() {
    let workspace = tempfile::tempdir().expect("workspace");
    write_entry(
        workspace.path(),
        "frontend",
        "2-no-lesson",
        &entry_file(2, "frontend", "no lesson", "no-observed-effect", None),
    );

    let registry = ExperienceRegistry::discover(workspace.path(), &[], None);
    let entry = registry.iter().next().expect("entry");
    assert!(entry.is_no_entry());
    let messages = catalog_messages(workspace.path(), &[], None, None, None);
    assert!(messages[0].message.text_content().contains("NO_ENTRY"));
}

#[test]
fn incomplete_lesson_is_skipped_with_a_warning() {
    let workspace = tempfile::tempdir().expect("workspace");
    let content = "---\nseq: 1\ndomain: rust\ntask: task\nconfidence: medium\n---\n\
                   ## Edits\ncomponent: a.md\nchange: b\nobserved: c\nverdict: helped\n\
                   ## Experience\ncue: only a cue\nlesson: missing evidence\n";
    write_entry(workspace.path(), "rust", "1-task", content);

    let registry = ExperienceRegistry::discover(workspace.path(), &[], None);
    assert!(registry.is_empty());
    assert!(!registry.warnings().is_empty(), "the skip is recorded");
}

#[test]
fn workspace_entry_wins_over_user_entry_with_the_same_key() {
    let workspace = tempfile::tempdir().expect("workspace");
    let user = tempfile::tempdir().expect("user");
    write_entry(
        workspace.path(),
        "rust",
        "1-task",
        &entry_file(1, "rust", "task", "helped", Some(("ws cue", "ws", "ws"))),
    );
    let user_path = user.path().join(USER_EXPERIENCE_DIR).join("rust/entries/1-task");
    std::fs::create_dir_all(&user_path).expect("dirs");
    std::fs::write(
        user_path.join(ENTRY_FILE),
        entry_file(1, "rust", "task", "helped", Some(("user cue", "user", "user"))),
    )
    .expect("write user");

    let registry = ExperienceRegistry::discover(workspace.path(), &[], Some(user.path()));
    assert_eq!(registry.len(), 1);
    let entry = registry.iter().next().expect("entry");
    assert_eq!(entry.scope, ExperienceScope::Workspace);
    assert_eq!(entry.cue.as_deref(), Some("ws cue"));
}

#[test]
fn streaming_retention_hides_the_current_and_later_sequences() {
    let workspace = tempfile::tempdir().expect("workspace");
    for seq in 1..=3 {
        write_entry(
            workspace.path(),
            "rust",
            &format!("{seq}-task"),
            &entry_file(seq, "rust", "task", "helped", Some(("cue", "lesson", "evidence"))),
        );
    }
    let registry = ExperienceRegistry::discover(workspace.path(), &[], None);
    let seqs: Vec<u64> =
        registry.filtered(None, Some(3)).into_iter().map(|entry| entry.seq).collect();
    assert_eq!(seqs, vec![2, 1], "newest visible first, current seq hidden");

    let messages = catalog_messages(workspace.path(), &[], None, None, Some(2));
    let text = messages[0].message.text_content();
    assert!(text.contains("#1"));
    assert!(!text.contains("#2"));
    assert!(!text.contains("#3"));
}

#[test]
fn domain_filter_scopes_the_catalog() {
    let workspace = tempfile::tempdir().expect("workspace");
    write_entry(
        workspace.path(),
        "rust",
        "1-task",
        &entry_file(1, "rust", "task", "helped", Some(("rust cue", "l", "e"))),
    );
    write_entry(
        workspace.path(),
        "frontend",
        "2-task",
        &entry_file(2, "frontend", "task", "helped", Some(("frontend cue", "l", "e"))),
    );

    let messages = catalog_messages(workspace.path(), &[], None, Some("rust"), None);
    let text = messages[0].message.text_content();
    assert!(text.contains("rust cue"));
    assert!(!text.contains("frontend cue"));
}

#[test]
fn catalog_is_bounded() {
    let workspace = tempfile::tempdir().expect("workspace");
    let long = "x".repeat(MAX_EXPERIENCE_SUMMARY_CHARS);
    for seq in 1..=40 {
        write_entry(
            workspace.path(),
            "rust",
            &format!("{seq}-task"),
            &entry_file(seq, "rust", "task", "helped", Some((&long, "l", "e"))),
        );
    }
    let messages = catalog_messages(workspace.path(), &[], None, None, None);
    let text = messages[0].message.text_content();
    let lines = text.lines().filter(|line| line.starts_with("- ")).count();
    assert!(lines < 40, "byte budget stops the catalog early");
    assert!(text.len() <= MAX_EXPERIENCE_CATALOG_BYTES + WORKSPACE_CATALOG_HEADER.len() + 64);
}

#[test]
fn entry_directory_and_verdict_must_match_the_schema() {
    let workspace = tempfile::tempdir().expect("workspace");
    write_entry(
        workspace.path(),
        "rust",
        "9-wrong-seq",
        &entry_file(1, "rust", "task", "helped", None),
    );
    write_entry(workspace.path(), "rust", "2-task", &entry_file(2, "rust", "task", "maybe", None));

    let registry = ExperienceRegistry::discover(workspace.path(), &[], None);
    assert!(registry.is_empty(), "both entries are invalid: {:?}", registry.warnings());
}

#[test]
fn save_entry_writes_the_harness_tree_and_rebuilds_the_index() {
    let workspace = tempfile::tempdir().expect("workspace");
    let entry = sample_entry(1, "rust-crates", "add parser coverage");
    let mut files = BTreeMap::new();
    files.insert("guardrails.md".to_string(), "Never force-push.\n".to_string());
    files.insert(
        "skills/atlas/SKILL.md".to_string(),
        "---\nname: atlas\ndescription: graph\n---\nbody\n".to_string(),
    );
    let harness = HarnessTree::new(files);

    let dir = save_entry(workspace.path(), &entry, Some(&harness)).expect("saved");
    assert!(dir.join(ENTRY_FILE).is_file());
    assert!(dir.join(HARNESS_DIR).join("guardrails.md").is_file());
    assert!(dir.join(HARNESS_DIR).join("skills/atlas/SKILL.md").is_file());
    let index = std::fs::read_to_string(
        workspace.path().join(WORKSPACE_EXPERIENCE_DIR).join("rust-crates").join(INDEX_FILE),
    )
    .expect("index");
    assert!(index.contains("#1"));
    assert!(index.contains("deliverables written but never reopened"));

    let registry = ExperienceRegistry::discover(workspace.path(), &[], None);
    let loaded = registry.iter().next().expect("reloaded");
    assert_eq!(loaded.lesson.as_deref(), Some("add a verify-after-write guardrail"));
    assert_eq!(loaded.display_path, "rust-crates/entries/1-add-parser-coverage/EXPERIENCE.md");
}

#[test]
fn rendered_entry_roundtrips_through_the_loader() {
    let workspace = tempfile::tempdir().expect("workspace");
    let entry = sample_entry(3, "rust", "roundtrip task");
    let text = render_entry_file(&entry);
    write_entry(workspace.path(), "rust", "3-roundtrip-task", &text);

    let registry = ExperienceRegistry::discover(workspace.path(), &[], None);
    let loaded = registry.iter().next().expect("loaded");
    assert_eq!(loaded.seq, 3);
    assert_eq!(loaded.domain, "rust");
    assert_eq!(loaded.edits, entry.edits);
    assert_eq!(loaded.cue, entry.cue);
    assert_eq!(loaded.lesson, entry.lesson);
    assert_eq!(loaded.evidence, entry.evidence);
}

#[test]
fn only_the_first_experience_block_is_kept() {
    let workspace = tempfile::tempdir().expect("workspace");
    let content = "---\nseq: 1\ndomain: rust\ntask: task\nconfidence: medium\n---\n\
                   ## Edits\ncomponent: a.md\nchange: b\nobserved: c\nverdict: helped\n\
                   ## Experience\ncue: first cue\nlesson: first lesson\nevidence: first evidence\n\
                   ## Experience\ncue: second cue\nlesson: second lesson\nevidence: second evidence\n";
    write_entry(workspace.path(), "rust", "1-task", content);

    let registry = ExperienceRegistry::discover(workspace.path(), &[], None);
    let entry = registry.iter().next().expect("entry");
    assert_eq!(entry.cue.as_deref(), Some("first cue"));
    assert_eq!(entry.lesson.as_deref(), Some("first lesson"));
}

#[test]
fn load_rejects_unknown_domains_and_sequences() {
    let workspace = tempfile::tempdir().expect("workspace");
    let error = load_experience("missing", None, workspace.path(), &[], None).expect_err("missing");
    assert!(error.contains("no experience entries"));
    assert!(load_experience("", None, workspace.path(), &[], None).is_err());
}

#[test]
fn oversized_entry_text_is_truncated_for_the_tool() {
    let workspace = tempfile::tempdir().expect("workspace");
    let evidence = "e".repeat(MAX_EXPERIENCE_TOOL_BYTES + 1024);
    write_entry(
        workspace.path(),
        "rust",
        "1-task",
        &entry_file(1, "rust", "task", "helped", Some(("cue", "lesson", &evidence))),
    );
    let loaded = load_experience("rust", None, workspace.path(), &[], None).expect("load");
    assert!(loaded.truncated);
    assert!(loaded.text.len() <= MAX_EXPERIENCE_TOOL_BYTES + 32);
    assert!(loaded.text.contains("[truncated]"));
}
