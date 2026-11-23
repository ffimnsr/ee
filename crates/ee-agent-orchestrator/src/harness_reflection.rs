//! Session reflection: curated evidence projection, output parser, and
//! approval-gated experience write.
//!
//! The reflector session sees a curated copy of the improvement record: the
//! chosen harness, the base harness for diffing, non-base candidate probes,
//! composition decisions, the anonymized adoption map with the selection
//! rationale, bounded tool records, and the workspace diff. Raw selection
//! internals are excluded by construction. Grader, rubric, and score material
//! is refused at projection build and again on the parsed lesson.
//!
//! The output contract is a tag-fenced reply: an `<edits>` block with one
//! five-line row per changed component and at most one `<experience>` block.
//! `NO_ENTRY` is the valid answer when no contrast supports a lesson.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use crate::harness_validation::HarnessTree;
use crate::user_experience::{
    EditRow, EditVerdict, ExperienceConfidence, ExperienceEntry, ExperienceScope, save_entry,
};

/// Maximum size of one reflection projection.
pub const MAX_PROJECTION_BYTES: usize = 160 * 1024;
/// Maximum bytes retained from one candidate rollout.
pub const MAX_ROLLOUT_BYTES: usize = 8 * 1024;
/// Maximum tool records included in one projection.
pub const MAX_TOOL_RECORDS: usize = 64;
/// Maximum bytes retained from the workspace diff.
pub const MAX_WORKSPACE_DIFF_BYTES: usize = 16 * 1024;
/// Maximum bytes retained from the task prompt in the projection.
const MAX_TASK_BYTES: usize = 8 * 1024;
/// Maximum bytes retained from composition decisions.
const MAX_DECISIONS_BYTES: usize = 16 * 1024;
/// Maximum bytes retained from the selection rationale.
const MAX_SELECTION_WHY_BYTES: usize = 4 * 1024;
/// Budget for one rendered harness (base or chosen).
const MAX_HARNESS_RENDER_BYTES: usize = 16 * 1024;
/// Maximum bytes retained from one harness file inside the projection.
const MAX_HARNESS_FILE_BYTES: usize = 4 * 1024;
/// Total budget for candidate rollout evidence.
const MAX_CANDIDATE_ROLLOUTS_BYTES: usize = 16 * 1024;
/// Total budget for candidate harness diffs.
const MAX_CANDIDATE_DIFFS_BYTES: usize = 16 * 1024;
/// Maximum bytes retained from one changed candidate file.
const MAX_CANDIDATE_DIFF_FILE_BYTES: usize = 4 * 1024;

/// Case-insensitive markers that indicate grader or scoring material.
const GRADER_MARKERS: &[&str] =
    &["grader", "rubric", "ground truth", "expected score", "score:", "graded"];

/// One candidate probe included in the improvement record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CandidateProbe {
    /// Stable candidate label (`ref`, `c1`, `composed`, ...).
    pub label: String,
    /// Bounded rollout text observed by the probe.
    pub rollout: String,
}

/// Anonymized adoption record retained for the reflector.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AdoptionRecord {
    /// Anonymous selector id to candidate label.
    pub anonymous_ids: BTreeMap<String, String>,
    /// The selection's rationale, already bounded by the selector contract.
    pub selection_why: String,
}

/// One observed tool execution.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReflectionToolRecord {
    /// Turn sequence.
    pub turn: u64,
    /// Model step inside the turn.
    pub iteration: u64,
    /// Tool name.
    pub tool: String,
    /// Whether the call succeeded.
    pub success: bool,
    /// Bounded result summary.
    pub summary: String,
}

/// Everything the reflector is allowed to see about one improvement run.
#[derive(Debug, Clone)]
pub struct ReflectionEvidence {
    /// Task description or prompt.
    pub task: String,
    /// Library domain.
    pub domain: String,
    /// Harness that was adopted.
    pub chosen_harness: HarnessTree,
    /// Harness before the edits, when a base copy exists.
    pub base_harness: Option<HarnessTree>,
    /// Non-base candidate probes.
    pub candidates: Vec<CandidateProbe>,
    /// Candidate harnesses, for per-candidate diffing against the base.
    pub candidate_harnesses: Vec<(String, HarnessTree)>,
    /// Composition notes.
    pub decisions: Option<String>,
    /// Anonymized adoption record.
    pub adoption: Option<AdoptionRecord>,
    /// Bounded tool records.
    pub tool_records: Vec<ReflectionToolRecord>,
    /// Workspace diff summary.
    pub workspace_diff: Option<String>,
    /// Raw selection internals. Never copied into the projection; retained so
    /// the exclusion is testable.
    pub raw_selection_record: Option<String>,
}

/// One durable lesson parsed from the reflector reply.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReflectionLesson {
    /// Trigger cue.
    pub cue: String,
    /// Durable lesson.
    pub lesson: String,
    /// Execution-contrast evidence.
    pub evidence: String,
    /// Optional confidence stated by the reflector; invalid values are
    /// rejected during parsing.
    pub confidence: Option<ExperienceConfidence>,
}

/// Parsed reflector reply.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReflectionOutput {
    /// One row per changed component.
    pub edits: Vec<EditRow>,
    /// At most one lesson; `None` is `NO_ENTRY`.
    pub experience: Option<ReflectionLesson>,
}

/// Builds the curated, bounded projection sent to the reflector.
pub fn build_projection(evidence: &ReflectionEvidence) -> Result<String, String> {
    let mut parts = Vec::new();
    parts.push(format!(
        "# Reflection Evidence\n\n## Task\n\n{}",
        truncate_chars(evidence.task.trim(), MAX_TASK_BYTES)
    ));
    parts.push(format!("## Domain\n\n{}", evidence.domain.trim()));
    if let Some(base) = &evidence.base_harness {
        parts.push(render_harness("Base Harness", base, MAX_HARNESS_RENDER_BYTES)?);
    }
    parts.push(render_harness(
        "Chosen Harness",
        &evidence.chosen_harness,
        MAX_HARNESS_RENDER_BYTES,
    )?);
    if let Some(base) = &evidence.base_harness {
        parts.push(render_diff(base, &evidence.chosen_harness));
    }
    let mut rollout_budget = MAX_CANDIDATE_ROLLOUTS_BYTES;
    for candidate in &evidence.candidates {
        assert_no_grader_material(&candidate.label, &candidate.rollout)?;
        if rollout_budget == 0 {
            continue;
        }
        let limit = rollout_budget.min(MAX_ROLLOUT_BYTES);
        let rollout = truncate_chars(candidate.rollout.trim(), limit);
        rollout_budget = rollout_budget.saturating_sub(rollout.len());
        parts.push(format!("## Candidate Probe `{}`\n\n{rollout}", candidate.label));
    }
    if let Some(base) = &evidence.base_harness {
        let mut diff_budget = MAX_CANDIDATE_DIFFS_BYTES;
        for (label, tree) in &evidence.candidate_harnesses {
            if diff_budget == 0 {
                break;
            }
            let diff = render_candidate_diff(base, tree, diff_budget)?;
            diff_budget = diff_budget.saturating_sub(diff.len());
            parts.push(format!("## Candidate Harness Diff `{label}`\n\n{diff}"));
        }
    }
    if let Some(decisions) = &evidence.decisions {
        assert_no_grader_material("composition decisions", decisions)?;
        parts.push(format!(
            "## Composition Decisions\n\n{}",
            truncate_chars(decisions.trim(), MAX_DECISIONS_BYTES)
        ));
    }
    if let Some(adoption) = &evidence.adoption {
        assert_no_grader_material("selection rationale", &adoption.selection_why)?;
        let mut map = String::new();
        for (anonymous, label) in &adoption.anonymous_ids {
            map.push_str(&format!("- {anonymous} -> {label}\n"));
        }
        parts.push(format!(
            "## Adoption Map\n\n{map}\n## Selection Rationale\n\n{}",
            truncate_chars(adoption.selection_why.trim(), MAX_SELECTION_WHY_BYTES)
        ));
    }
    if !evidence.tool_records.is_empty() {
        let mut records = String::new();
        for record in evidence.tool_records.iter().take(MAX_TOOL_RECORDS) {
            assert_no_grader_material("tool record", &record.summary)?;
            records.push_str(&format!(
                "- turn {} step {} `{}` {}: {}\n",
                record.turn,
                record.iteration,
                record.tool,
                if record.success { "ok" } else { "failed" },
                truncate_chars(record.summary.trim(), 240)
            ));
        }
        parts.push(format!("## Tool Records\n\n{records}"));
    }
    if let Some(diff) = &evidence.workspace_diff {
        assert_no_grader_material("workspace diff", diff)?;
        parts.push(format!(
            "## Workspace Diff\n\n{}",
            truncate_chars(diff.trim(), MAX_WORKSPACE_DIFF_BYTES)
        ));
    }
    // Raw selection internals are deliberately absent: the reflector audits the
    // recorded rationale, never the selector's private record.
    let projection = parts.join("\n\n");
    if projection.len() > MAX_PROJECTION_BYTES {
        return Err(format!(
            "reflection projection is {} bytes, over the {MAX_PROJECTION_BYTES}-byte bound",
            projection.len()
        ));
    }
    Ok(projection)
}

fn render_harness(title: &str, tree: &HarnessTree, max_bytes: usize) -> Result<String, String> {
    let mut text = format!("## {title}\n");
    let mut used = 0usize;
    for (path, content) in &tree.files {
        assert_no_grader_material(path, content)?;
        if used >= max_bytes {
            text.push_str("\n… [harness truncated]\n");
            break;
        }
        let budget = max_bytes.saturating_sub(used);
        let body = truncate_chars(content.trim_end(), MAX_HARNESS_FILE_BYTES.min(budget));
        let block = format!("\n### {path}\n\n```markdown\n{body}\n```\n");
        used = used.saturating_add(block.len());
        text.push_str(&block);
    }
    Ok(text)
}

fn render_diff(base: &HarnessTree, chosen: &HarnessTree) -> String {
    let paths: BTreeSet<&String> = base.files.keys().chain(chosen.files.keys()).collect();
    let mut lines = Vec::new();
    for path in paths {
        match (base.files.get(path), chosen.files.get(path)) {
            (Some(before), Some(after)) if before == after => {}
            (Some(_), Some(_)) => lines.push(format!("- ~ {path}")),
            (Some(_), None) => lines.push(format!("- - {path}")),
            (None, Some(_)) => lines.push(format!("- + {path}")),
            (None, None) => {}
        }
    }
    if lines.is_empty() { "- no component changes".to_string() } else { lines.join("\n") }
}

/// Renders a candidate's changed component bodies against the base harness.
///
/// A path-only diff cannot support per-edit verdicts: the reflector must see
/// what changed. Bodies are bounded per file so one candidate cannot consume
/// the whole projection budget.
fn render_candidate_diff(
    base: &HarnessTree,
    candidate: &HarnessTree,
    max_bytes: usize,
) -> Result<String, String> {
    let paths: BTreeSet<&String> = base.files.keys().chain(candidate.files.keys()).collect();
    let mut out = String::new();
    for path in paths {
        if out.len() >= max_bytes {
            out.push_str("\n… [candidate diff truncated]\n");
            break;
        }
        let before = base.files.get(path);
        let after = candidate.files.get(path);
        if before == after {
            continue;
        }
        let content = after.or(before).map(String::as_str).unwrap_or_default();
        assert_no_grader_material(path, content)?;
        let marker = match (before, after) {
            (Some(_), Some(_)) => "~",
            (Some(_), None) => "-",
            (None, Some(_)) => "+",
            (None, None) => continue,
        };
        out.push_str(&format!("\n### {marker} {path}\n\n"));
        if after.is_some() {
            let budget = max_bytes.saturating_sub(out.len());
            out.push_str("```markdown\n");
            out.push_str(&truncate_chars(
                content.trim_end(),
                MAX_CANDIDATE_DIFF_FILE_BYTES.min(budget),
            ));
            out.push_str("\n```\n");
        }
    }
    if out.is_empty() { Ok(String::from("no component changes")) } else { Ok(out) }
}

/// Public leak guard shared with the harnessing context builder.
pub fn refuse_grader_material(label: &str, text: &str) -> Result<(), String> {
    assert_no_grader_material(label, text)
}

fn assert_no_grader_material(label: &str, text: &str) -> Result<(), String> {
    let lowered = text.to_ascii_lowercase();
    for marker in GRADER_MARKERS {
        if lowered.contains(marker) {
            return Err(format!(
                "reflection evidence refused: {label} contains grader/scoring material ({marker:?})"
            ));
        }
    }
    Ok(())
}

/// Parses the reflector reply.
pub fn parse_reflection(reply: &str) -> Result<ReflectionOutput, String> {
    let edits_blocks = tagged_blocks(reply, "edits");
    if edits_blocks.len() != 1 {
        return Err(format!(
            "reflector emitted {} <edits> blocks; exactly one is required",
            edits_blocks.len()
        ));
    }
    let edits = parse_edit_rows(&edits_blocks[0])?;
    let experience_blocks = tagged_blocks(reply, "experience");
    if experience_blocks.len() > 1 {
        return Err(format!(
            "reflector emitted {} <experience> blocks; at most one lesson per task is allowed",
            experience_blocks.len()
        ));
    }
    let experience =
        experience_blocks.first().and_then(|block| parse_lesson(block).transpose()).transpose()?;
    for edit in &edits {
        assert_no_grader_material(
            "edit row",
            &format!("{} {} {}", edit.change, edit.observed, edit.component),
        )?;
    }
    if let Some(lesson) = &experience {
        assert_no_grader_material(
            "parsed lesson",
            &format!("{} {} {}", lesson.cue, lesson.lesson, lesson.evidence),
        )?;
    }
    Ok(ReflectionOutput { edits, experience })
}

fn tagged_blocks(reply: &str, tag: &str) -> Vec<String> {
    let open = format!("<{tag}>");
    let close = format!("</{tag}>");
    let mut blocks = Vec::new();
    let mut rest = reply;
    while let Some(start) = rest.find(&open) {
        let after = &rest[start + open.len()..];
        let Some(end) = after.find(&close) else { break };
        blocks.push(after[..end].trim().to_string());
        rest = &after[end + close.len()..];
    }
    blocks
}

fn parse_edit_rows(block: &str) -> Result<Vec<EditRow>, String> {
    let mut rows = Vec::new();
    for chunk in block.split("\n\n") {
        let mut fields: BTreeMap<String, String> = BTreeMap::new();
        for line in chunk.lines() {
            let Some((key, value)) = line.split_once(':') else { continue };
            fields.insert(key.trim().to_ascii_lowercase(), value.trim().to_string());
        }
        if fields.is_empty() {
            continue;
        }
        let required = |key: &str| -> Result<String, String> {
            fields
                .get(key)
                .map(|value| value.trim().to_string())
                .filter(|value| !value.is_empty())
                .ok_or_else(|| format!("edit row is missing `{key}`"))
        };
        let verdict: EditVerdict = required("verdict")?.parse()?;
        let candidates = fields
            .get("candidates")
            .map(|value| {
                value
                    .split(',')
                    .map(str::trim)
                    .filter(|candidate| !candidate.is_empty())
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default();
        rows.push(EditRow {
            component: required("component")?,
            candidates,
            change: required("change")?,
            observed: required("observed")?,
            verdict,
        });
    }
    if rows.is_empty() {
        return Err(String::from("`<edits>` has no rows"));
    }
    Ok(rows)
}

fn parse_lesson(block: &str) -> Result<Option<ReflectionLesson>, String> {
    if block.to_ascii_lowercase().contains("no_entry") {
        return Ok(None);
    }
    let mut fields: BTreeMap<String, String> = BTreeMap::new();
    for line in block.lines() {
        let Some((key, value)) = line.split_once(':') else { continue };
        fields.insert(key.trim().to_ascii_lowercase(), value.trim().to_string());
    }
    let required = |key: &str| -> Result<String, String> {
        fields
            .get(key)
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty())
            .ok_or_else(|| format!("experience block is missing non-empty `{key}`"))
    };
    let confidence = match fields.get("confidence").map(|value| value.trim()) {
        None | Some("") => None,
        Some(value) => Some(value.parse::<ExperienceConfidence>()?),
    };
    Ok(Some(ReflectionLesson {
        cue: required("cue")?,
        lesson: required("lesson")?,
        evidence: required("evidence")?,
        confidence,
    }))
}

/// Builds one library entry from a parsed reply.
pub fn build_entry(
    output: &ReflectionOutput,
    seq: u64,
    domain: &str,
    task: &str,
    confidence: ExperienceConfidence,
) -> Result<ExperienceEntry, String> {
    if output.edits.is_empty() {
        return Err(String::from("reflection has no edit rows"));
    }
    let confidence =
        output.experience.as_ref().and_then(|lesson| lesson.confidence).unwrap_or(confidence);
    let mut entry = ExperienceEntry {
        seq,
        domain: domain.to_string(),
        task: task.to_string(),
        confidence,
        edits: output.edits.clone(),
        cue: output.experience.as_ref().map(|lesson| lesson.cue.clone()),
        lesson: output.experience.as_ref().map(|lesson| lesson.lesson.clone()),
        evidence: output.experience.as_ref().map(|lesson| lesson.evidence.clone()),
        content: String::new(),
        scope: ExperienceScope::Workspace,
        path: PathBuf::new(),
        display_path: String::new(),
    };
    entry.content = crate::user_experience::render_entry_file(&entry);
    Ok(entry)
}

/// Writes a parsed reflection only after explicit approval.
pub fn write_reflection(
    workspace_root: &Path,
    entry: &ExperienceEntry,
    harness: Option<&HarnessTree>,
    approved: bool,
) -> Result<PathBuf, String> {
    if !approved {
        return Err(String::from("reflection write declined"));
    }
    save_entry(workspace_root, entry, harness)
}

fn truncate_chars(text: &str, max_bytes: usize) -> String {
    if text.len() <= max_bytes {
        return text.to_string();
    }
    let mut cut = max_bytes.min(text.len());
    while cut > 0 && !text.is_char_boundary(cut) {
        cut -= 1;
    }
    format!("{}\n… [truncated]", &text[..cut])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tree(files: &[(&str, &str)]) -> HarnessTree {
        HarnessTree::new(
            files
                .iter()
                .map(|(path, content)| ((*path).to_string(), (*content).to_string()))
                .collect(),
        )
    }

    fn evidence() -> ReflectionEvidence {
        ReflectionEvidence {
            task: "add parser coverage".to_string(),
            domain: "rust-crates".to_string(),
            chosen_harness: tree(&[("guardrails.md", "verify writes\n")]),
            base_harness: Some(tree(&[("guardrails.md", "base\n")])),
            candidates: vec![CandidateProbe {
                label: "c1".to_string(),
                rollout: "reopened outputs at step 11".to_string(),
            }],
            candidate_harnesses: vec![(
                "c1".to_string(),
                tree(&[("guardrails.md", "verify writes\n")]),
            )],
            decisions: Some("kept verify rule".to_string()),
            adoption: Some(AdoptionRecord {
                anonymous_ids: BTreeMap::from([("a1".to_string(), "c1".to_string())]),
                selection_why: "delivery criterion decided it".to_string(),
            }),
            tool_records: vec![ReflectionToolRecord {
                turn: 1,
                iteration: 2,
                tool: "write_file".to_string(),
                success: true,
                summary: "wrote parser.rs".to_string(),
            }],
            workspace_diff: Some("+ src/parser.rs".to_string()),
            raw_selection_record: Some("selection_internal_token=secret".to_string()),
        }
    }

    fn reply(experience: &str) -> String {
        format!(
            "<edits>\ncomponent: tool_guidance.md\ncandidates: c1\nchange: added verify rule\nobserved: candidate reopened outputs (run c2, step 11)\nverdict: helped\n</edits>\n<experience>\n{experience}\n</experience>"
        )
    }

    #[test]
    fn projection_includes_evidence_and_excludes_raw_selection_internals() {
        let projection = build_projection(&evidence()).expect("projection");
        assert!(projection.contains("add parser coverage"));
        assert!(projection.contains("Candidate Probe `c1`"));
        assert!(projection.contains("Candidate Harness Diff `c1`"));
        assert!(projection.contains("Adoption Map"));
        assert!(projection.contains("Tool Records"));
        assert!(projection.contains("Workspace Diff"));
        assert!(!projection.contains("selection_internal_token"));
    }

    #[test]
    fn projection_refuses_grader_material() {
        let mut record = evidence();
        record.candidates[0].rollout = "expected score: 0.9 from the grader".to_string();
        let error = build_projection(&record).expect_err("grader refused");
        assert!(error.contains("grader"), "{error}");
    }

    #[test]
    fn projection_is_bounded() {
        let mut record = evidence();
        record.candidates[0].rollout = "x".repeat(MAX_ROLLOUT_BYTES * 12);
        let projection = build_projection(&record).expect("projection");
        assert!(projection.len() <= MAX_PROJECTION_BYTES);
    }

    #[test]
    fn parser_roundtrips_edits_and_one_lesson() {
        let output = parse_reflection(&reply(
            "cue: writes not reopened\nlesson: add verify rule\nevidence: c2 vs base b1",
        ))
        .expect("parses");
        assert_eq!(output.edits.len(), 1);
        assert_eq!(output.edits[0].verdict, EditVerdict::Helped);
        assert_eq!(output.edits[0].candidates, vec!["c1".to_string()]);
        let lesson = output.experience.expect("lesson");
        assert_eq!(lesson.cue, "writes not reopened");
        assert_eq!(lesson.confidence, None);
    }

    #[test]
    fn parsed_confidence_is_validated_and_overrides_the_default() {
        let output = parse_reflection(&reply(
            "cue: cue\nlesson: lesson\nevidence: evidence\nconfidence: high",
        ))
        .expect("parses");
        let entry =
            build_entry(&output, 1, "rust", "task", ExperienceConfidence::Low).expect("entry");
        assert_eq!(entry.confidence, ExperienceConfidence::High);

        let invalid = reply("cue: cue\nlesson: lesson\nevidence: evidence\nconfidence: certain");
        assert!(parse_reflection(&invalid).is_err());
    }

    #[test]
    fn no_entry_reply_keeps_edits_only() {
        let output = parse_reflection(&reply("NO_ENTRY")).expect("parses");
        assert!(output.experience.is_none());
        assert_eq!(output.edits.len(), 1);
    }

    #[test]
    fn multiple_experience_blocks_are_rejected() {
        let text = format!(
            "{}\n<experience>\ncue: second\nlesson: second\nevidence: second\n</experience>",
            reply("cue: first\nlesson: first\nevidence: first")
        );
        let error = parse_reflection(&text).expect_err("one lesson bound");
        assert!(error.contains("at most one"), "{error}");
    }

    #[test]
    fn exactly_one_edits_block_is_required() {
        let text = format!("{}\n{}", reply("NO_ENTRY"), reply("NO_ENTRY"));
        let error = parse_reflection(&text).expect_err("one edits block");
        assert!(error.contains("exactly one"), "{error}");
        assert!(parse_reflection("no blocks").is_err());
    }

    #[test]
    fn invalid_vocabulary_and_incomplete_lessons_are_rejected() {
        let bad_verdict = reply("NO_ENTRY").replace("verdict: helped", "verdict: maybe");
        assert!(parse_reflection(&bad_verdict).is_err());

        let incomplete = reply("cue: only\nlesson: partial");
        assert!(parse_reflection(&incomplete).is_err());
    }

    #[test]
    fn grader_material_in_the_parsed_lesson_is_rejected() {
        let text = reply("cue: cue\nlesson: expected score 95\nevidence: contrast");
        let error = parse_reflection(&text).expect_err("leak guard");
        assert!(error.contains("grader"), "{error}");
    }

    #[test]
    fn declined_write_leaves_no_entry() {
        let workspace = tempfile::tempdir().expect("workspace");
        let output = parse_reflection(&reply("cue: cue\nlesson: lesson\nevidence: evidence"))
            .expect("parses");
        let entry =
            build_entry(&output, 1, "rust", "task", ExperienceConfidence::Medium).expect("entry");
        let error = write_reflection(workspace.path(), &entry, None, false).expect_err("declined");
        assert!(error.contains("declined"));
        assert!(!workspace.path().join(".agents/experience").exists());
    }

    #[test]
    fn no_entry_is_written_with_edits_only() {
        let workspace = tempfile::tempdir().expect("workspace");
        let output = parse_reflection(&reply("NO_ENTRY")).expect("parses");
        let entry =
            build_entry(&output, 1, "rust", "task", ExperienceConfidence::Low).expect("entry");
        let dir = write_reflection(workspace.path(), &entry, None, true).expect("write");
        let text = std::fs::read_to_string(dir.join("EXPERIENCE.md")).expect("entry file");
        assert!(text.contains("NO_ENTRY"));
        assert!(!text.contains("lesson:"));
        let registry =
            crate::user_experience::ExperienceRegistry::discover(workspace.path(), &[], None);
        let loaded = registry.iter().next().expect("reloaded");
        assert!(loaded.is_no_entry());
    }

    #[test]
    fn approved_write_stores_the_entry_and_harness_tree() {
        let workspace = tempfile::tempdir().expect("workspace");
        let output = parse_reflection(&reply(
            "cue: writes not reopened\nlesson: add verify rule\nevidence: c2 vs base b1",
        ))
        .expect("parses");
        let entry =
            build_entry(&output, 1, "rust", "task", ExperienceConfidence::Medium).expect("entry");
        let harness = tree(&[("guardrails.md", "verify writes\n")]);
        let dir = write_reflection(workspace.path(), &entry, Some(&harness), true).expect("write");
        assert!(dir.join("EXPERIENCE.md").is_file());
        assert!(dir.join("harness/guardrails.md").is_file());
        let registry =
            crate::user_experience::ExperienceRegistry::discover(workspace.path(), &[], None);
        assert_eq!(registry.len(), 1);
    }
}
