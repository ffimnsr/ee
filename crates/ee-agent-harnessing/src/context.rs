//! Improver context-directory builder.
//!
//! Each stage gets a bounded, redacted directory: task, source files, harness
//! trees, rollout views, deliverables, library cues, decisions, and a mission
//! that lists only the directories actually written. Every text is redacted
//! through the shared remote redactor and refused if it carries grader,
//! rubric, or score material. A build with no renderable trajectory and no
//! deliverable fails loudly: adapting on nothing is a bug, not a no-op.

use std::path::{Path, PathBuf};

use ee_agent_orchestrator::harness_reflection::refuse_grader_material;
use ee_agent_orchestrator::harness_validation::{HarnessTree, materialize};
use ee_agent_orchestrator::rollout_view::RolloutView;
use ee_agent_remote::redact::Redactor;

use crate::channel::Deliverable;
use crate::error::{HarnessError, HarnessResult};
use crate::library_context::{LibraryEvidence, safe_relative_path};
use crate::task::{HarnessTask, render_deliverable};

/// Maximum bytes written to one context markdown file.
pub const MAX_CONTEXT_FILE_BYTES: usize = 64 * 1024;
/// Maximum bytes written to the observations file. Reflection projections are
/// larger than ordinary evidence files because they carry the candidate pool.
pub const MAX_OBSERVATIONS_BYTES: usize = 160 * 1024;
/// Maximum bytes retained from one rollout in a context file.
pub const MAX_ROLLOUT_BYTES: usize = 32 * 1024;
/// Maximum bytes retained from one deliverable body.
pub const MAX_DELIVERABLE_BYTES: usize = 16 * 1024;
/// Safety cap for one untrimmed selector artifact.
pub const MAX_SELECTOR_CANDIDATE_BYTES: usize = 256 * 1024;

/// One anonymized selector candidate.
#[derive(Debug, Clone)]
pub struct SelectorCandidate {
    /// Anonymous presentation id (`a1`, `a2`, ...).
    pub anon: String,
    /// Deliverable observed for the candidate.
    pub deliverable: Deliverable,
    /// Bounded trajectory text, when the task has no file deliverable.
    pub trajectory: Option<String>,
}

/// Evidence for one stage context.
#[derive(Debug, Clone)]
pub struct StageEvidence {
    /// Stage subdirectory name (`propose-inv-0`, `compose-0`, ...).
    pub stage: String,
    /// Mission/contract text for the stage.
    pub mission: String,
    /// Task under improvement.
    pub task: HarnessTask,
    /// Named harness trees (for example `base`, `candidate-inv-0`).
    pub harnesses: Vec<(String, HarnessTree)>,
    /// Rollout views by label.
    pub rollouts: Vec<(String, RolloutView)>,
    /// Deliverables by label.
    pub deliverables: Vec<(String, Deliverable)>,
    /// Bounded prior experience entries with their archived harness trees.
    pub library: LibraryEvidence,
    /// Composition or stage decisions, keyed by their source label.
    pub decisions: Vec<(String, String)>,
    /// Anonymized selector candidates, written untrimmed with size headers.
    pub selector: Vec<SelectorCandidate>,
    /// Additional bounded observations (validator errors, notes).
    pub observations: Option<String>,
}

/// Builds one stage context directory and returns its path.
pub fn build(root: &Path, evidence: &StageEvidence) -> HarnessResult<PathBuf> {
    let redactor = Redactor::default();
    let directory = root.join("context").join(&evidence.stage);
    std::fs::create_dir_all(&directory)?;
    let mut present = Vec::new();

    let task_text = format!(
        "# Task\n\nid: {}\ndomain: {}\n\n## Prompt\n\n{}\n\n## Source\n\n{}",
        evidence.task.id,
        evidence.task.domain,
        evidence.task.prompt,
        evidence
            .task
            .source_files()
            .iter()
            .map(|file| format!("- {}", file.name))
            .collect::<Vec<_>>()
            .join("\n")
    );
    write_checked(&directory.join("task.md"), &task_text, &redactor)?;

    let sources = evidence.task.source_files();
    if !sources.is_empty() {
        let source_dir = directory.join("source");
        std::fs::create_dir_all(&source_dir)?;
        for file in &sources {
            let path = source_dir.join(safe_name(&file.name));
            write_checked(&path, &file.content, &redactor)?;
        }
        present.push("source/");
    }

    if !evidence.harnesses.is_empty() {
        let harness_dir = directory.join("harness");
        std::fs::create_dir_all(&harness_dir)?;
        for (name, tree) in &evidence.harnesses {
            let redacted = redacted_tree(tree, &redactor);
            let target = harness_dir.join(safe_name(name));
            materialize(&redacted, &target).map_err(HarnessError::Validation)?;
            for content in redacted.files.values() {
                refuse_grader_material("harness copy", content).map_err(HarnessError::Parse)?;
            }
        }
        present.push("harness/");
    }

    if !evidence.rollouts.is_empty() {
        let rollout_dir = directory.join("rollouts");
        std::fs::create_dir_all(&rollout_dir)?;
        for (label, view) in &evidence.rollouts {
            if !view.has_trajectory() {
                continue;
            }
            let text = view.render(MAX_ROLLOUT_BYTES);
            write_checked(&rollout_dir.join(format!("{}.md", safe_name(label))), &text, &redactor)?;
        }
        if rollout_dir.read_dir().map(|mut entries| entries.next().is_some()).unwrap_or(false) {
            present.push("rollouts/");
        }
    }

    if !evidence.deliverables.is_empty() {
        let deliverable_dir = directory.join("deliverables");
        std::fs::create_dir_all(&deliverable_dir)?;
        for (label, deliverable) in &evidence.deliverables {
            let text = render_deliverable(deliverable, MAX_DELIVERABLE_BYTES);
            write_checked(
                &deliverable_dir.join(format!("{}.md", safe_name(label))),
                &text,
                &redactor,
            )?;
        }
        present.push("deliverables/");
    }

    if !evidence.library.is_empty() {
        let library_dir = directory.join("library");
        std::fs::create_dir_all(&library_dir)?;
        let mut index = String::from("# Library evidence\n\n");
        if evidence.library.cues.is_empty() {
            index.push_str("_No prior entry could be copied._\n");
        } else {
            index.push_str(&evidence.library.cues.join("\n"));
            index.push('\n');
        }
        if !evidence.library.skipped.is_empty() {
            index.push_str("\n## Skipped entries\n\n");
            for note in &evidence.library.skipped {
                index.push_str(&format!("- {note}\n"));
            }
        }
        write_checked(&library_dir.join("INDEX.md"), &index, &redactor)?;
        for entry in &evidence.library.entries {
            let entry_dir =
                library_dir.join(safe_name(&entry.domain)).join(safe_name(&entry.directory));
            write_checked(&entry_dir.join("EXPERIENCE.md"), &entry.experience, &redactor)?;
            for (relative, content) in &entry.harness.files {
                if !safe_relative_path(relative) {
                    return Err(HarnessError::State(format!(
                        "library harness path escapes the entry: {relative}"
                    )));
                }
                write_checked(&entry_dir.join("harness").join(relative), content, &redactor)?;
            }
        }
        present.push("library/");
    }

    if !evidence.decisions.is_empty() {
        let decisions_dir = directory.join("decisions");
        std::fs::create_dir_all(&decisions_dir)?;
        for (label, decisions) in &evidence.decisions {
            write_checked(
                &decisions_dir.join(format!("{}.md", safe_name(label))),
                decisions,
                &redactor,
            )?;
        }
        present.push("decisions/");
    }
    if !evidence.selector.is_empty() {
        let selector_dir = directory.join("selector");
        std::fs::create_dir_all(&selector_dir)?;
        for candidate in &evidence.selector {
            let text = render_selector_candidate(candidate);
            write_checked_with(
                &selector_dir.join(format!("{}.md", safe_name(&candidate.anon))),
                &text,
                &redactor,
                MAX_SELECTOR_CANDIDATE_BYTES,
            )?;
        }
        present.push("selector/");
    }
    if let Some(observations) = &evidence.observations {
        write_checked_with(
            &directory.join("observations.md"),
            observations,
            &redactor,
            MAX_OBSERVATIONS_BYTES,
        )?;
        present.push("observations.md");
    }

    if present.is_empty() {
        return Err(HarnessError::State(
            "no renderable trajectory, deliverable, or harness evidence; adapting on nothing"
                .into(),
        ));
    }
    let has_trajectory = evidence.rollouts.iter().any(|(_, view)| view.has_trajectory());
    if !has_trajectory
        && evidence.deliverables.is_empty()
        && evidence.selector.is_empty()
        && evidence.observations.is_none()
    {
        return Err(HarnessError::State(
            "no rollout has a renderable trajectory and no deliverable, selector candidate, or \
             observation exists; the proposer would be adapting on nothing"
                .into(),
        ));
    }

    let mission = format!(
        "# Mission\n\n{}\n\n## Evidence present\n\n{}\n",
        evidence.mission.trim(),
        present.iter().map(|entry| format!("- {entry}")).collect::<Vec<_>>().join("\n")
    );
    write_checked(&directory.join("mission.md"), &mission, &redactor)?;
    Ok(directory)
}

fn redacted_tree(tree: &HarnessTree, redactor: &Redactor) -> HarnessTree {
    HarnessTree::new(
        tree.files.iter().map(|(path, content)| (path.clone(), redactor.text(content))).collect(),
    )
}

fn render_selector_candidate(candidate: &SelectorCandidate) -> String {
    let mut text = format!(
        "# Candidate {}\n\nCOMPLETE — nothing trimmed; navigate with grep/offsets\n\n\
         deliverable text: {} bytes\n",
        candidate.anon,
        candidate.deliverable.text.len()
    );
    if candidate.deliverable.text.trim().is_empty() {
        text.push_str("\n_no text deliverable_\n");
    } else {
        text.push_str("\n## Deliverable\n\n");
        text.push_str(&candidate.deliverable.text);
        text.push('\n');
    }
    if !candidate.deliverable.assets.is_empty() {
        text.push_str("\n## OPEN THESE\n\nRendered behavior beats reading source.\n\n");
        for asset in &candidate.deliverable.assets {
            text.push_str(&format!("- {} ({} bytes)\n", asset.name, asset.bytes));
            if let Some(content) = &asset.content {
                text.push_str("```\n");
                text.push_str(content);
                text.push_str("\n```\n");
            }
        }
    }
    if let Some(trajectory) = &candidate.trajectory {
        text.push_str("\n## Trajectory\n\n");
        text.push_str(trajectory);
        text.push('\n');
    }
    text
}

fn write_checked(path: &Path, text: &str, redactor: &Redactor) -> HarnessResult<()> {
    write_checked_with(path, text, redactor, MAX_CONTEXT_FILE_BYTES)
}

fn write_checked_with(
    path: &Path,
    text: &str,
    redactor: &Redactor,
    max_bytes: usize,
) -> HarnessResult<()> {
    let redacted = redactor.text(text);
    refuse_grader_material(&path.display().to_string(), &redacted).map_err(HarnessError::Parse)?;
    let bounded = truncate(&redacted, max_bytes);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, bounded)?;
    Ok(())
}

fn truncate(text: &str, max_bytes: usize) -> String {
    if text.len() <= max_bytes {
        return text.to_string();
    }
    let mut cut = max_bytes.min(text.len());
    while cut > 0 && !text.is_char_boundary(cut) {
        cut -= 1;
    }
    format!("{}\n… [truncated]\n", &text[..cut])
}

fn safe_name(name: &str) -> String {
    let cleaned: String = name
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, '.' | '_' | '-') {
                character
            } else {
                '_'
            }
        })
        .collect();
    if cleaned.trim_matches('_').is_empty() { String::from("item") } else { cleaned }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::channel::DeliverableAsset;
    use crate::library_context::LibraryEntry;
    use std::collections::BTreeMap;

    fn task() -> HarnessTask {
        HarnessTask::new("t1", "improve it", "rust", std::env::temp_dir())
    }

    fn view() -> RolloutView {
        RolloutView {
            session_id: "s".into(),
            turns: vec![ee_agent_orchestrator::rollout_view::RolloutTurn {
                turn: 1,
                stop_reason: Some("end_turn".into()),
                tools: vec![ee_agent_orchestrator::rollout_view::RolloutToolRecord {
                    turn: 1,
                    iteration: 1,
                    tool: "write_file".into(),
                    success: true,
                    error_kind: None,
                    duration_ms: 4,
                }],
            }],
            ..RolloutView::default()
        }
    }

    fn base_evidence() -> StageEvidence {
        StageEvidence {
            stage: "propose-inv-0".into(),
            mission: "Improve the process.".into(),
            task: task(),
            harnesses: vec![("base".into(), HarnessTree::default())],
            rollouts: vec![("ref-0".into(), view())],
            deliverables: vec![(
                "ref-0".into(),
                Deliverable {
                    text: "done".into(),
                    assets: vec![DeliverableAsset {
                        name: "out.txt".into(),
                        bytes: 4,
                        content: None,
                    }],
                },
            )],
            library: LibraryEvidence {
                cues: vec!["- [rust #1] cue".into()],
                entries: vec![LibraryEntry {
                    domain: "rust".into(),
                    seq: 1,
                    directory: "1-prior".into(),
                    experience: "# Experience\n\nlesson\n".into(),
                    harness: HarnessTree::new(BTreeMap::from([(
                        "guardrails.md".into(),
                        "prior rule\n".into(),
                    )])),
                }],
                skipped: Vec::new(),
            },
            decisions: vec![(String::from("step-0"), String::from("- kept base"))],
            selector: Vec::new(),
            observations: None,
        }
    }

    #[test]
    fn builds_the_curated_directory_and_present_list() {
        let root = tempfile::tempdir().expect("root");
        let directory = build(root.path(), &base_evidence()).expect("build");
        for file in [
            "task.md",
            "mission.md",
            "decisions/step-0.md",
            "rollouts/ref-0.md",
            "deliverables/ref-0.md",
            "library/INDEX.md",
            "library/rust/1-prior/EXPERIENCE.md",
            "library/rust/1-prior/harness/guardrails.md",
        ] {
            assert!(directory.join(file).is_file(), "missing {file}");
        }
        let mission = std::fs::read_to_string(directory.join("mission.md")).expect("mission");
        assert!(mission.contains("rollouts/"));
        assert!(mission.contains("deliverables/"));
        assert!(mission.contains("decisions/"));
        assert!(mission.contains("library/"));
        assert!(!mission.contains("observations.md"));
    }

    #[test]
    fn refuses_grader_material() {
        let root = tempfile::tempdir().expect("root");
        let mut evidence = base_evidence();
        evidence.library.cues.push("expected score: 0.9".into());
        let error = build(root.path(), &evidence).expect_err("leak guard");
        assert!(error.to_string().contains("grader"), "{error}");
    }

    #[test]
    fn refuses_grader_material_in_library_harness() {
        let root = tempfile::tempdir().expect("root");
        let mut evidence = base_evidence();
        evidence.library.cues.clear();
        evidence.library.entries[0]
            .harness
            .files
            .insert("scripts/check.sh".into(), "expected score: 0.9".into());
        let error = build(root.path(), &evidence).expect_err("leak guard");
        assert!(error.to_string().contains("grader"), "{error}");
    }

    #[test]
    fn redacts_credentials() {
        let root = tempfile::tempdir().expect("root");
        let mut evidence = base_evidence();
        evidence.decisions =
            vec![(String::from("step-0"), String::from("api_key = sk-live-secret-value"))];
        let directory = build(root.path(), &evidence).expect("build");
        let decisions =
            std::fs::read_to_string(directory.join("decisions/step-0.md")).expect("read");
        assert!(!decisions.contains("sk-live-secret-value"));
    }

    #[test]
    fn empty_evidence_is_a_loud_error() {
        let root = tempfile::tempdir().expect("root");
        let workspace = root.path().join("workspace");
        std::fs::create_dir_all(&workspace).expect("workspace");
        let mut evidence = base_evidence();
        evidence.task = HarnessTask::new("t1", "improve it", "rust", &workspace);
        evidence.rollouts.clear();
        evidence.deliverables.clear();
        evidence.harnesses.clear();
        evidence.library = LibraryEvidence::default();
        evidence.decisions.clear();
        let error = build(root.path(), &evidence).expect_err("nothing to adapt");
        assert!(error.to_string().contains("adapting on nothing"), "{error}");
    }

    #[test]
    fn oversized_deliverables_render_names_only() {
        let root = tempfile::tempdir().expect("root");
        let mut evidence = base_evidence();
        evidence.deliverables = vec![(
            "big".into(),
            Deliverable {
                text: "x".repeat(MAX_DELIVERABLE_BYTES * 2),
                assets: vec![DeliverableAsset {
                    name: "artifact.bin".into(),
                    bytes: 1024,
                    content: None,
                }],
            },
        )];
        let directory = build(root.path(), &evidence).expect("build");
        let text = std::fs::read_to_string(directory.join("deliverables/big.md")).expect("read");
        assert!(text.contains("names-only"));
        assert!(text.contains("artifact.bin"));
    }

    #[test]
    fn selector_artifacts_are_untrimmed_with_size_headers() {
        let root = tempfile::tempdir().expect("root");
        let mut evidence = base_evidence();
        evidence.deliverables.clear();
        evidence.selector = vec![SelectorCandidate {
            anon: "a1".into(),
            deliverable: Deliverable {
                text: "y".repeat(MAX_DELIVERABLE_BYTES * 2),
                assets: vec![DeliverableAsset {
                    name: "report.md".into(),
                    bytes: 32,
                    content: Some("# rendered\n".into()),
                }],
            },
            trajectory: Some("## Turn 1\n- `write_file` ok\n".into()),
        }];
        let directory = build(root.path(), &evidence).expect("build");
        let text = std::fs::read_to_string(directory.join("selector/a1.md")).expect("read");
        assert!(text.contains("COMPLETE — nothing trimmed"));
        assert!(text.len() > MAX_DELIVERABLE_BYTES);
        assert!(text.contains("OPEN THESE"));
        assert!(text.contains("## Trajectory"));
        assert!(!text.contains("names-only"));
    }

    #[test]
    fn grader_material_is_refused_in_harness_copies() {
        let root = tempfile::tempdir().expect("root");
        let mut evidence = base_evidence();
        let mut files = std::collections::BTreeMap::new();
        files.insert("guardrails.md".to_string(), "expected score: 10".to_string());
        evidence.harnesses = vec![("base".into(), HarnessTree::new(files))];
        let error = build(root.path(), &evidence).expect_err("harness leak guard");
        assert!(error.to_string().contains("grader"), "{error}");
    }
}
