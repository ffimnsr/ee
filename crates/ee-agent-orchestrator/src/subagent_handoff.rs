//! Bounded, structured handoff from generic subagents to their parent.
//!
//! Child model output is parsed before any truncation. Model-claimed citations
//! remain distinct from backend-observed execution evidence. Malformed output
//! fails closed and remains quarantined. Rubber-duck reports use their own
//! stricter contract and never pass through this module.

use serde::{Deserialize, Serialize};

use crate::delegation_quality::{FindingEvidence, SubagentFinding};
use crate::subagent_verifier::{SubagentCitations, SubagentEvidence};
use crate::tasks::truncate;

/// Current generic subagent handoff schema.
pub const SUBAGENT_HANDOFF_SCHEMA_VERSION: u32 = 1;
/// Maximum serialized handoff sent to a parent.
pub const MAX_SUBAGENT_HANDOFF_BYTES: usize = 16 * 1024;
/// Maximum summary length before final payload pruning.
pub const MAX_HANDOFF_SUMMARY_CHARS: usize = 2_000;
/// Maximum findings retained.
pub const MAX_HANDOFF_FINDINGS: usize = 16;
/// Maximum unresolved items and recommended actions retained per collection.
pub const MAX_HANDOFF_LIST_ITEMS: usize = 8;
/// Maximum observed or claimed items retained per evidence collection.
pub const MAX_HANDOFF_EVIDENCE_ITEMS: usize = 64;
/// Maximum generic list/evidence item length.
pub const MAX_HANDOFF_ITEM_CHARS: usize = 512;
/// Maximum finding key length.
pub const MAX_HANDOFF_FINDING_KEY_CHARS: usize = 128;
/// Maximum finding claim length.
pub const MAX_HANDOFF_FINDING_CLAIM_CHARS: usize = 1_000;

/// Instructions appended to every generic child role.
pub const GENERIC_HANDOFF_INSTRUCTIONS: &str = r#"Return exactly one JSON object for parent handoff. No markdown or hidden reasoning. Shape: {"schema_version":1,"summary":"concise result","findings":[{"key":"stable-topic","claim":"narrow claim","kind":"observed|inference","evidence":[{"file":"path"}|{"tool":"name"}],"confidence":"low|medium|high","rejected_alternatives":[],"recommended_next_action":"next step"}],"citations":{"files":["path"],"tools":["name"]},"unresolved":[],"recommended_actions":[]}. Cite only files and tools actually observed during this task. Use unresolved for partial work."#;

/// Backend-authoritative terminal outcome of one subagent run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum SubagentStatus {
    /// Child loop ended normally and passed verification.
    Completed,
    /// Child loop or handoff verification failed.
    Failed,
    /// Child loop was cancelled.
    Cancelled,
}

/// How backend obtained parent handoff payload.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HandoffOutputFormat {
    /// Child output parsed as generic handoff JSON.
    Structured,
    /// Child output violated generic handoff contract and was rejected.
    RejectedMalformed,
    /// No child output exists because execution failed or was cancelled.
    BackendTerminal,
}

/// Parent-visible generic subagent handoff.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct SubagentHandoff {
    /// Handoff schema version.
    pub schema_version: u32,
    /// Backend-selected child role.
    pub role: String,
    /// Stable backend child id.
    pub subagent_id: String,
    /// Backend-authoritative terminal status.
    pub status: SubagentStatus,
    /// Concise result summary.
    pub summary: String,
    /// Structured findings using delegation-quality schema.
    pub findings: Vec<SubagentFinding>,
    /// Claims parsed from raw child output before truncation.
    pub claimed_citations: SubagentCitations,
    /// Successful file/tool activity projected by backend execution log.
    pub observed_evidence: SubagentEvidence,
    /// Incomplete work or unanswered questions.
    pub unresolved: Vec<String>,
    /// Suggested parent follow-up.
    pub recommended_actions: Vec<String>,
    /// Parsing path used for child output.
    pub output_format: HandoffOutputFormat,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct HandoffDraft {
    schema_version: u32,
    summary: String,
    findings: Vec<SubagentFinding>,
    citations: SubagentCitations,
    unresolved: Vec<String>,
    recommended_actions: Vec<String>,
}

impl SubagentHandoff {
    /// Parses completed generic child output before applying any bounds.
    #[must_use]
    pub fn from_completed_output(
        role: &str,
        subagent_id: &str,
        raw_output: &str,
        observed_evidence: SubagentEvidence,
    ) -> Self {
        let raw_citations = SubagentCitations::extract(raw_output);
        let parsed = parse_draft(raw_output);
        let (
            status,
            summary,
            findings,
            mut citations,
            unresolved,
            recommended_actions,
            output_format,
        ) = match parsed {
            Some(draft) if draft.schema_version == SUBAGENT_HANDOFF_SCHEMA_VERSION => (
                SubagentStatus::Completed,
                draft.summary,
                draft.findings,
                draft.citations,
                draft.unresolved,
                draft.recommended_actions,
                HandoffOutputFormat::Structured,
            ),
            _ => (
                SubagentStatus::Failed,
                String::new(),
                Vec::new(),
                SubagentCitations::default(),
                Vec::new(),
                Vec::new(),
                HandoffOutputFormat::RejectedMalformed,
            ),
        };
        merge_citations(&mut citations, raw_citations);
        for finding in &findings {
            for evidence in &finding.evidence {
                match evidence {
                    FindingEvidence::File(path) => push_unique(&mut citations.files, path.clone()),
                    FindingEvidence::Tool(name) => push_unique(&mut citations.tools, name.clone()),
                }
            }
        }
        Self {
            schema_version: SUBAGENT_HANDOFF_SCHEMA_VERSION,
            role: role.to_string(),
            subagent_id: subagent_id.to_string(),
            status,
            summary,
            findings,
            claimed_citations: citations,
            observed_evidence,
            unresolved,
            recommended_actions,
            output_format,
        }
        .bounded()
    }

    /// Builds failed/cancelled backend terminal envelope.
    #[must_use]
    pub fn terminal(role: &str, subagent_id: &str, status: SubagentStatus) -> Self {
        Self {
            schema_version: SUBAGENT_HANDOFF_SCHEMA_VERSION,
            role: role.to_string(),
            subagent_id: subagent_id.to_string(),
            status,
            summary: String::new(),
            findings: Vec::new(),
            claimed_citations: SubagentCitations::default(),
            observed_evidence: SubagentEvidence::default(),
            unresolved: Vec::new(),
            recommended_actions: Vec::new(),
            output_format: HandoffOutputFormat::BackendTerminal,
        }
        .bounded()
    }

    /// Deterministic bounded JSON sent to parent tool/transcript surfaces.
    ///
    /// Rebounds a clone so public mutation or direct deserialization cannot
    /// bypass payload limits at parent-facing serialization boundaries.
    pub fn to_json(&self) -> Result<String, serde_json::Error> {
        serde_json::to_string(&self.clone().bounded())
    }

    /// Validates durable or injected handoffs without silently normalizing
    /// attacker-controlled identity, schema, or oversized fields.
    pub(crate) fn validate_integrity(&self) -> Result<(), String> {
        if self.schema_version != SUBAGENT_HANDOFF_SCHEMA_VERSION {
            return Err(format!(
                "unsupported subagent handoff schema {} (expected {})",
                self.schema_version, SUBAGENT_HANDOFF_SCHEMA_VERSION
            ));
        }
        if self.clone().bounded() != *self {
            return Err(format!(
                "subagent handoff is not canonically bounded to {MAX_SUBAGENT_HANDOFF_BYTES} bytes"
            ));
        }
        match (self.output_format, self.status) {
            (HandoffOutputFormat::Structured, SubagentStatus::Completed)
            | (HandoffOutputFormat::RejectedMalformed, SubagentStatus::Failed)
            | (
                HandoffOutputFormat::BackendTerminal,
                SubagentStatus::Failed | SubagentStatus::Cancelled,
            ) => Ok(()),
            (HandoffOutputFormat::Structured, _) => {
                Err("structured subagent handoff must have completed status".into())
            }
            (HandoffOutputFormat::RejectedMalformed, _) => {
                Err("malformed subagent handoff must have failed status".into())
            }
            (HandoffOutputFormat::BackendTerminal, SubagentStatus::Completed) => {
                Err("backend-terminal subagent handoff cannot have completed status".into())
            }
        }
    }

    fn bounded(mut self) -> Self {
        self.role = truncate(&self.role, MAX_HANDOFF_FINDING_KEY_CHARS);
        self.subagent_id = truncate(&self.subagent_id, MAX_HANDOFF_FINDING_KEY_CHARS);
        self.summary = truncate(&self.summary, MAX_HANDOFF_SUMMARY_CHARS);
        self.findings.truncate(MAX_HANDOFF_FINDINGS);
        for finding in &mut self.findings {
            finding.key = truncate(&finding.key, MAX_HANDOFF_FINDING_KEY_CHARS);
            finding.claim = truncate(&finding.claim, MAX_HANDOFF_FINDING_CLAIM_CHARS);
            finding.evidence.truncate(MAX_HANDOFF_LIST_ITEMS);
            for evidence in &mut finding.evidence {
                match evidence {
                    FindingEvidence::File(value) | FindingEvidence::Tool(value) => {
                        *value = truncate(value, MAX_HANDOFF_ITEM_CHARS);
                    }
                }
            }
            finding.rejected_alternatives.truncate(MAX_HANDOFF_LIST_ITEMS);
            bound_strings(&mut finding.rejected_alternatives, MAX_HANDOFF_ITEM_CHARS);
            finding.recommended_next_action =
                truncate(&finding.recommended_next_action, MAX_HANDOFF_ITEM_CHARS);
        }
        bound_evidence(&mut self.claimed_citations.files);
        bound_evidence(&mut self.claimed_citations.tools);
        bound_evidence(&mut self.observed_evidence.files_accessed);
        bound_evidence(&mut self.observed_evidence.tools_executed);
        self.unresolved.truncate(MAX_HANDOFF_LIST_ITEMS);
        self.recommended_actions.truncate(MAX_HANDOFF_LIST_ITEMS);
        bound_strings(&mut self.unresolved, MAX_HANDOFF_ITEM_CHARS);
        bound_strings(&mut self.recommended_actions, MAX_HANDOFF_ITEM_CHARS);
        self.prune_to_serialized_cap();
        self
    }

    fn prune_to_serialized_cap(&mut self) {
        while serialized_len(self) > MAX_SUBAGENT_HANDOFF_BYTES {
            if self.findings.iter_mut().rev().any(|finding| {
                if finding.rejected_alternatives.is_empty() {
                    false
                } else {
                    finding.rejected_alternatives.pop();
                    true
                }
            }) {
                continue;
            }
            if self.recommended_actions.pop().is_some() || self.unresolved.pop().is_some() {
                continue;
            }
            if self.findings.pop().is_some() {
                continue;
            }
            if self.observed_evidence.tools_executed.len() > 1 {
                self.observed_evidence.tools_executed.pop();
                continue;
            }
            if self.observed_evidence.files_accessed.len() > 1 {
                self.observed_evidence.files_accessed.pop();
                continue;
            }
            if self.claimed_citations.tools.len() > 1 {
                self.claimed_citations.tools.pop();
                continue;
            }
            if self.claimed_citations.files.len() > 1 {
                self.claimed_citations.files.pop();
                continue;
            }
            let chars = self.summary.chars().count();
            if chars == 0 {
                break;
            }
            self.summary = truncate(&self.summary, chars / 2);
        }
    }
}

fn parse_draft(raw: &str) -> Option<HandoffDraft> {
    serde_json::from_str(raw.trim()).ok()
}

fn merge_citations(target: &mut SubagentCitations, source: SubagentCitations) {
    for file in source.files {
        push_unique(&mut target.files, file);
    }
    for tool in source.tools {
        push_unique(&mut target.tools, tool);
    }
}

fn push_unique(values: &mut Vec<String>, value: String) {
    if !values.contains(&value) {
        values.push(value);
    }
}

fn bound_evidence(values: &mut Vec<String>) {
    values.truncate(MAX_HANDOFF_EVIDENCE_ITEMS);
    bound_strings(values, MAX_HANDOFF_ITEM_CHARS);
}

fn bound_strings(values: &mut [String], max_chars: usize) {
    for value in values {
        *value = truncate(value, max_chars);
    }
}

fn serialized_len(handoff: &SubagentHandoff) -> usize {
    serde_json::to_vec(handoff).map_or(usize::MAX, |bytes| bytes.len())
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn structured_handoff_roundtrips_with_backend_identity_and_evidence() {
        let raw = json!({
            "schema_version": 1,
            "summary": "found issue",
            "findings": [{
                "key": "bug",
                "claim": "wrong branch",
                "kind": "observed",
                "evidence": [{"file": "/work/a.rs"}],
                "confidence": "high",
                "rejected_alternatives": [],
                "recommended_next_action": "fix branch"
            }],
            "citations": {"files": ["/work/a.rs"], "tools": ["read_file"]},
            "unresolved": ["tests not run"],
            "recommended_actions": ["run tests"]
        })
        .to_string();
        let observed = SubagentEvidence {
            files_accessed: vec!["/work/a.rs".into()],
            tools_executed: vec!["read_file".into()],
        };
        let handoff = SubagentHandoff::from_completed_output("reviewer", "task-2", &raw, observed);
        assert_eq!(handoff.output_format, HandoffOutputFormat::Structured);
        assert_eq!(handoff.role, "reviewer");
        assert_eq!(handoff.subagent_id, "task-2");
        assert_eq!(handoff.findings.len(), 1);
        assert_eq!(handoff.observed_evidence.tools_executed, vec!["read_file"]);
        let decoded: SubagentHandoff = serde_json::from_str(&handoff.to_json().unwrap()).unwrap();
        assert_eq!(decoded, handoff);
    }

    #[test]
    fn citation_after_oversized_summary_survives_pre_truncation_extraction() {
        let raw = json!({
            "schema_version": 1,
            "summary": "x".repeat(20_000),
            "findings": [],
            "citations": {"files": ["/work/late.rs"], "tools": []},
            "unresolved": [],
            "recommended_actions": []
        })
        .to_string();
        let handoff = SubagentHandoff::from_completed_output(
            "worker",
            "task-2",
            &raw,
            SubagentEvidence::default(),
        );
        assert!(handoff.summary.chars().count() <= MAX_HANDOFF_SUMMARY_CHARS + 1);
        assert_eq!(handoff.claimed_citations.files, vec!["/work/late.rs"]);
    }

    #[test]
    fn malformed_or_incomplete_output_fails_closed_without_exposing_raw_text() {
        for raw in ["not json [file:/work/a.rs]", r#"{"schema_version":1}"#] {
            let handoff = SubagentHandoff::from_completed_output(
                "worker",
                "task-2",
                raw,
                SubagentEvidence::default(),
            );
            assert_eq!(handoff.status, SubagentStatus::Failed);
            assert_eq!(handoff.output_format, HandoffOutputFormat::RejectedMalformed);
            assert!(handoff.summary.is_empty());
        }
    }

    #[test]
    fn completed_backend_terminal_envelope_fails_integrity_validation() {
        let handoff = SubagentHandoff::terminal("worker", "task-2", SubagentStatus::Completed);
        assert!(
            handoff
                .validate_integrity()
                .expect_err("completed backend terminal rejected")
                .contains("cannot have completed status")
        );
    }

    #[test]
    fn parent_serialization_rebounds_mutated_or_deserialized_payload() {
        let mut handoff = SubagentHandoff::terminal("worker", "task-2", SubagentStatus::Failed);
        handoff.summary = "x".repeat(MAX_SUBAGENT_HANDOFF_BYTES * 2);
        handoff.unresolved = vec!["y".repeat(MAX_HANDOFF_ITEM_CHARS * 4); 100];

        assert!(handoff.validate_integrity().is_err());
        let json = handoff.to_json().expect("bounded serialization");
        assert!(json.len() <= MAX_SUBAGENT_HANDOFF_BYTES);
        let decoded: SubagentHandoff = serde_json::from_str(&json).expect("bounded handoff JSON");
        assert!(decoded.validate_integrity().is_ok());
    }

    #[test]
    fn oversized_payload_is_deterministically_pruned_under_cap() {
        let findings = (0..100)
            .map(|index| {
                json!({
                    "key": format!("key-{index}"),
                    "claim": "c".repeat(5_000),
                    "kind": "inference",
                    "evidence": [{"tool": "t".repeat(2_000)}],
                    "confidence": "medium",
                    "rejected_alternatives": vec!["a".repeat(2_000); 20],
                    "recommended_next_action": "n".repeat(2_000)
                })
            })
            .collect::<Vec<_>>();
        let raw = json!({
            "schema_version": 1,
            "summary": "s".repeat(30_000),
            "findings": findings,
            "citations": {"files": vec!["f".repeat(2_000); 100], "tools": vec!["t".repeat(2_000); 100]},
            "unresolved": vec!["u".repeat(2_000); 100],
            "recommended_actions": vec!["a".repeat(2_000); 100]
        })
        .to_string();
        let handoff = SubagentHandoff::from_completed_output(
            "worker",
            "task-2",
            &raw,
            SubagentEvidence {
                files_accessed: vec!["f".repeat(2_000); 100],
                tools_executed: vec!["t".repeat(2_000); 100],
            },
        );
        assert!(handoff.to_json().unwrap().len() <= MAX_SUBAGENT_HANDOFF_BYTES);
        assert!(handoff.findings.len() <= MAX_HANDOFF_FINDINGS);
        assert!(handoff.observed_evidence.files_accessed.len() <= MAX_HANDOFF_EVIDENCE_ITEMS);
    }

    #[test]
    fn terminal_envelope_bounds_backend_identity() {
        let handoff = SubagentHandoff::terminal(
            &"r".repeat(10_000),
            &"i".repeat(10_000),
            SubagentStatus::Failed,
        );
        assert!(handoff.role.chars().count() <= MAX_HANDOFF_FINDING_KEY_CHARS + 1);
        assert!(handoff.subagent_id.chars().count() <= MAX_HANDOFF_FINDING_KEY_CHARS + 1);
        assert!(handoff.to_json().unwrap().len() <= MAX_SUBAGENT_HANDOFF_BYTES);
    }

    #[test]
    fn partial_result_keeps_unresolved_and_actions() {
        let raw = json!({
            "schema_version": 1,
            "summary": "partial",
            "findings": [],
            "citations": {"files": [], "tools": []},
            "unresolved": ["missing fixture"],
            "recommended_actions": ["provide fixture"]
        })
        .to_string();
        let handoff = SubagentHandoff::from_completed_output(
            "summarizer",
            "task-2",
            &raw,
            SubagentEvidence::default(),
        );
        assert_eq!(handoff.unresolved, vec!["missing fixture"]);
        assert_eq!(handoff.recommended_actions, vec!["provide fixture"]);
    }
}
