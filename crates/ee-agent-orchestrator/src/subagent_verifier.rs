//! Subagent result verification and quarantine.
//!
//! The parent must verify a child's summary before merging its memory into
//! durable state: evidence-requiring roles (all built-ins except the
//! summarizer) must cite the files and tools their summary claims, and every
//! citation must appear in the child's observed execution log.  Failed,
//! cancelled, and unverified child output is quarantined instead of merged —
//! quarantined output never reaches the model context, but a bounded
//! summary stays inspectable by the parent.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::delegation_quality::{
    DELEGATION_QUALITY_SCHEMA_VERSION, ReportEvidence, SubagentReport, SubagentReportVerifier,
};
use crate::memory::MemoryStore;
use crate::subagent_handoff::SubagentStatus;
use crate::subagents::{SUBAGENT_SUMMARY_MAX_CHARS, SubagentId, SubagentResult, SubagentRole};
use crate::tasks::truncate;
use crate::tools::{SideEffectClass, ToolExecutionLogEntry};

/// Default maximum cited files per summary.
pub const DEFAULT_MAX_CITED_FILES: usize = 64;
/// Default maximum cited tools per summary.
pub const DEFAULT_MAX_CITED_TOOLS: usize = 64;
/// Cap on evidence entries collected from a child execution log.
pub const MAX_EVIDENCE_FILES: usize = 256;
/// Cap on distinct tools collected from a child execution log.
pub const MAX_EVIDENCE_TOOLS: usize = 256;
/// Cap on one citation marker token's length.
pub const MAX_CITATION_TOKEN_CHARS: usize = 512;

/// Observed activity of one child run, projected from its execution log.
///
/// Evidence records successful observations only, in first-seen order,
/// deduplicated and bounded; failed or denied attempts cannot prove claims.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct SubagentEvidence {
    /// Absolute file paths the child attempted to read or write.
    pub files_accessed: Vec<String>,
    /// Tool names the child attempted to execute.
    pub tools_executed: Vec<String>,
}

impl SubagentEvidence {
    /// Builds evidence from successful child execution entries: `path`
    /// arguments of read/write-class entries become accessed files, successful
    /// entries contribute tool names, both deduplicated and bounded.
    #[must_use]
    pub fn from_execution_log(log: &[ToolExecutionLogEntry]) -> Self {
        let mut files = Vec::new();
        let mut tools = Vec::new();
        for entry in log {
            if !entry.success {
                continue;
            }
            if !tools.contains(&entry.tool_name) && tools.len() < MAX_EVIDENCE_TOOLS {
                tools.push(entry.tool_name.clone());
            }
            if matches!(
                entry.side_effect_class,
                Some(SideEffectClass::Read | SideEffectClass::Write)
            ) && let Some(path) = entry.arguments.get("path").and_then(serde_json::Value::as_str)
                && !files.contains(&path.to_string())
                && files.len() < MAX_EVIDENCE_FILES
            {
                files.push(path.to_string());
            }
        }
        Self { files_accessed: files, tools_executed: tools }
    }
}

/// Citations a subagent summary claims, checked against [`SubagentEvidence`].
///
/// Structured citations can be attached directly; when produced by a child
/// loop, they are extracted from the summary text's `[file:path]` and
/// `[tool:name]` markers (the convention the built-in role instructions
/// teach).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct SubagentCitations {
    /// Cited absolute file paths.
    pub files: Vec<String>,
    /// Cited tool names.
    pub tools: Vec<String>,
}

impl SubagentCitations {
    /// Extracts bounded, deduplicated citations from `[file:...]` and
    /// `[tool:...]` markers in a summary, in first-seen order.
    #[must_use]
    pub fn extract(text: &str) -> Self {
        let mut files = Vec::new();
        let mut tools = Vec::new();
        let bytes = text.as_bytes();
        let mut index = 0usize;
        while index < bytes.len() {
            if let Some((marker_len, sink, capacity)) = scan_marker(&bytes[index..]) {
                // `scan_marker` returned where the token starts; parse until
                // the closing bracket.
                let start = index + marker_len;
                if let Some(relative_end) = find_closing(&bytes[start..]) {
                    let token = text[start..start + relative_end].trim();
                    if !token.is_empty()
                        && sink == CitationSink::Files
                        && !files.contains(&token.to_string())
                        && files.len() < capacity
                    {
                        files.push(token.to_string());
                    } else if !token.is_empty()
                        && sink == CitationSink::Tools
                        && !tools.contains(&token.to_string())
                        && tools.len() < capacity
                    {
                        tools.push(token.to_string());
                    }
                    index = start + relative_end + 1;
                    continue;
                }
            }
            index += 1;
        }
        Self { files, tools }
    }

    /// Whether any citation is present.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.files.is_empty() && self.tools.is_empty()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CitationSink {
    Files,
    Tools,
}

/// Bounded marker classifier: returns `(bytes before the token, sink, cap)`
/// when the slice starts with a well-formed `[file:` or `[tool:` marker.
fn scan_marker(bytes: &[u8]) -> Option<(usize, CitationSink, usize)> {
    if bytes.starts_with(b"[file:") {
        Some((b"[file:".len(), CitationSink::Files, DEFAULT_MAX_CITED_FILES))
    } else if bytes.starts_with(b"[tool:") {
        Some((b"[tool:".len(), CitationSink::Tools, DEFAULT_MAX_CITED_TOOLS))
    } else {
        None
    }
}

/// Position of the next `]` byte, capped at `MAX_CITATION_TOKEN_CHARS`.
fn find_closing(bytes: &[u8]) -> Option<usize> {
    let end = bytes.len().min(MAX_CITATION_TOKEN_CHARS);
    bytes[..end].iter().position(|byte| *byte == b']')
}

/// Outcome of one verification.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct SubagentVerification {
    /// Whether the result may be merged into parent memory.
    pub verified: bool,
    /// Citations missing from the child evidence (`file:...`, `tool:...`).
    pub missing_citations: Vec<String>,
    /// Why verification failed, when it did.
    pub rejected_reason: Option<String>,
}

impl SubagentVerification {
    fn verified() -> Self {
        Self { verified: true, missing_citations: Vec::new(), rejected_reason: None }
    }

    fn rejected(reason: impl Into<String>) -> Self {
        Self {
            verified: false,
            missing_citations: Vec::new(),
            rejected_reason: Some(reason.into()),
        }
    }
}

/// Verifies child summaries against observed evidence before memory merge.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubagentResultVerifier {
    max_cited_files: usize,
    max_cited_tools: usize,
}

impl Default for SubagentResultVerifier {
    fn default() -> Self {
        Self::new()
    }
}

impl SubagentResultVerifier {
    /// Creates a verifier with the default citation caps.
    #[must_use]
    pub fn new() -> Self {
        Self { max_cited_files: DEFAULT_MAX_CITED_FILES, max_cited_tools: DEFAULT_MAX_CITED_TOOLS }
    }

    /// Creates a verifier with explicit citation caps (used by tests).
    #[must_use]
    pub fn with_limits(max_cited_files: usize, max_cited_tools: usize) -> Self {
        Self { max_cited_files, max_cited_tools }
    }

    /// Whether a role's summaries must cite evidence; built-in roles except
    /// the summarizer do.
    #[must_use]
    pub fn role_requires_evidence(&self, role: &SubagentRole) -> bool {
        role.requires_evidence
    }

    /// Verifies a child result: only completed results of evidence-requiring
    /// roles are checked, every citation must exist in the child evidence,
    /// and the citation counts must stay within the configured caps.
    #[must_use]
    pub fn verify(
        &self,
        role: &SubagentRole,
        result: &SubagentResult,
        evidence: &SubagentEvidence,
    ) -> SubagentVerification {
        if result.handoff.status != SubagentStatus::Completed {
            return SubagentVerification::rejected(format!(
                "child did not complete (status {:?})",
                result.handoff.status
            ));
        }
        if !self.role_requires_evidence(role) {
            return SubagentVerification::verified();
        }
        if !result.handoff.findings.is_empty() {
            let report = SubagentReport {
                schema_version: DELEGATION_QUALITY_SCHEMA_VERSION,
                role: result.handoff.role.clone(),
                subagent_id: result.handoff.subagent_id.clone(),
                findings: result.handoff.findings.clone(),
            };
            let verification = SubagentReportVerifier
                .verify(&report, &ReportEvidence::from_subagent_evidence(evidence));
            if !verification.accepted {
                return SubagentVerification::rejected(format!(
                    "structured findings rejected: {}",
                    verification.rejected_reasons.join("; ")
                ));
            }
        }
        if result.handoff.claimed_citations.is_empty() {
            return SubagentVerification::rejected(
                "summary includes no cited files or tools".to_string(),
            );
        }
        if result.handoff.claimed_citations.files.len() > self.max_cited_files {
            return SubagentVerification::rejected(format!(
                "too many cited files ({} > {})",
                result.handoff.claimed_citations.files.len(),
                self.max_cited_files
            ));
        }
        if result.handoff.claimed_citations.tools.len() > self.max_cited_tools {
            return SubagentVerification::rejected(format!(
                "too many cited tools ({} > {})",
                result.handoff.claimed_citations.tools.len(),
                self.max_cited_tools
            ));
        }
        let mut missing = Vec::new();
        for file in &result.handoff.claimed_citations.files {
            if !evidence.files_accessed.contains(file) {
                missing.push(format!("file:{file}"));
            }
        }
        for tool in &result.handoff.claimed_citations.tools {
            if !evidence.tools_executed.contains(tool) {
                missing.push(format!("tool:{tool}"));
            }
        }
        if missing.is_empty() {
            SubagentVerification::verified()
        } else {
            let reason = format!("missing citations: {}", missing.join(", "));
            SubagentVerification {
                verified: false,
                missing_citations: missing,
                rejected_reason: Some(reason),
            }
        }
    }

    /// Verifies and, when verified, merges the child's produced memory items
    /// into `memory` (sensitive items are still skipped by the store).  The
    /// verification outcome is returned; rejected merges never touch the
    /// store.
    #[must_use]
    pub fn verify_and_merge(
        &self,
        memory: &mut MemoryStore,
        role: &SubagentRole,
        result: &SubagentResult,
        evidence: &SubagentEvidence,
    ) -> SubagentVerification {
        let verification = self.verify(role, result, evidence);
        if verification.verified {
            crate::subagents::merge_memory_items(memory, &result.produced_memory_items);
        }
        verification
    }
}

/// Bounded record of one quarantined subagent outcome.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct QuarantineEntry {
    /// The quarantined subagent.
    pub subagent_id: SubagentId,
    /// Terminal status at quarantine time.
    pub status: SubagentStatus,
    /// Bounded summary (empty for failures).
    pub summary: String,
    /// Bounded error summary, when the child failed or was cancelled.
    pub error_summary: Option<String>,
    /// Why the output was quarantined (`failed`, `cancelled`, or an
    /// unverified-summary reason).
    pub reason: String,
}

/// Quarantine state for failed, cancelled, and unverified child output.
///
/// Quarantined output is never merged into the memory store, so it cannot
/// reach the model context; [`SubagentQuarantine::quarantine_summary`]
/// exposes a bounded view the parent model may inspect.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SubagentQuarantine {
    entries: BTreeMap<SubagentId, QuarantineEntry>,
}

impl SubagentQuarantine {
    /// Quarantines a child outcome with a deterministic reason, replacing any
    /// prior entry for the same subagent.
    pub fn quarantine(&mut self, result: &SubagentResult, reason: impl Into<String>) {
        let entry = QuarantineEntry {
            subagent_id: result.subagent_id.clone(),
            status: result.handoff.status,
            summary: truncate(&result.handoff.summary, SUBAGENT_SUMMARY_MAX_CHARS),
            error_summary: result
                .error_summary
                .clone()
                .map(|text| truncate(&text, SUBAGENT_SUMMARY_MAX_CHARS)),
            reason: reason.into(),
        };
        self.entries.insert(entry.subagent_id.clone(), entry);
    }

    /// Whether a subagent's output is quarantined.
    #[must_use]
    pub fn is_quarantined(&self, subagent_id: &SubagentId) -> bool {
        self.entries.contains_key(subagent_id)
    }

    /// The bounded entry for a subagent, if quarantined.
    #[must_use]
    pub fn entry(&self, subagent_id: &SubagentId) -> Option<&QuarantineEntry> {
        self.entries.get(subagent_id)
    }

    /// All entries in stable subagent-id order.
    #[must_use]
    pub fn entries(&self) -> Vec<&QuarantineEntry> {
        self.entries.values().collect()
    }

    /// Bounded multi-line summary for the parent model (`subagent <id>
    /// (<status>): <reason>`, plus the error summary when present), truncated
    /// to `max_chars`.  `None` when nothing is quarantined.
    #[must_use]
    pub fn quarantine_summary(&self, max_chars: usize) -> Option<String> {
        if self.entries.is_empty() {
            return None;
        }
        let mut lines = Vec::new();
        for entry in self.entries.values() {
            let status = match entry.status {
                SubagentStatus::Completed => "completed",
                SubagentStatus::Failed => "failed",
                SubagentStatus::Cancelled => "cancelled",
            };
            let mut line = format!("subagent {} ({status}): {}", entry.subagent_id, entry.reason);
            if let Some(error) = &entry.error_summary {
                line.push_str(" — ");
                line.push_str(error);
            }
            lines.push(line);
        }
        Some(truncate(&lines.join("\n"), max_chars))
    }

    /// Number of quarantined subagents.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether nothing is quarantined.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Clears all quarantine state.
    pub fn clear(&mut self) {
        self.entries.clear();
    }
}

#[cfg(test)]
mod tests {
    use crate::memory::MemoryItem;
    use crate::subagent_handoff::SubagentHandoff;

    use super::*;

    fn completed_result(
        summary: &str,
        citations: SubagentCitations,
        memory_items: Vec<MemoryItem>,
    ) -> SubagentResult {
        let output = serde_json::json!({
            "schema_version": 1,
            "summary": summary,
            "findings": [],
            "citations": {"files": [], "tools": []},
            "unresolved": [],
            "recommended_actions": []
        })
        .to_string();
        let mut handoff = SubagentHandoff::from_completed_output(
            "worker",
            "task-2",
            &output,
            SubagentEvidence::default(),
        );
        handoff.claimed_citations = citations;
        SubagentResult {
            subagent_id: SubagentId::new("task-2"),
            handoff,
            produced_memory_items: memory_items,
            tool_call_count: 2,
            error_summary: None,
        }
    }

    fn read_entry(path: &str, tool: &str) -> ToolExecutionLogEntry {
        ToolExecutionLogEntry {
            tool_call_id: "tc-1".into(),
            tool_name: tool.into(),
            side_effect_class: Some(SideEffectClass::Read),
            arguments: serde_json::json!({ "path": path }),
            success: true,
            summary: "content".into(),
        }
    }

    fn evidence() -> SubagentEvidence {
        SubagentEvidence::from_execution_log(&[
            read_entry("/work/a.rs", "read_file"),
            read_entry("/work/b.rs", "read_file"),
            ToolExecutionLogEntry {
                tool_call_id: "tc-3".into(),
                tool_name: "grep".into(),
                side_effect_class: Some(SideEffectClass::Read),
                arguments: serde_json::json!({}),
                success: true,
                summary: "hits".into(),
            },
        ])
    }

    fn researcher() -> SubagentRole {
        crate::subagent_roles::BuiltinSubagentRole::Researcher.role()
    }

    fn summarizer() -> SubagentRole {
        crate::subagent_roles::BuiltinSubagentRole::Summarizer.role()
    }

    #[test]
    fn evidence_projects_files_and_tools_deduplicated_in_order() {
        let log = vec![
            read_entry("/work/a.rs", "read_file"),
            read_entry("/work/a.rs", "read_file"),
            read_entry("/work/b.rs", "write_file"),
            ToolExecutionLogEntry {
                tool_call_id: "tc-4".into(),
                tool_name: "write_file".into(),
                side_effect_class: Some(SideEffectClass::Write),
                arguments: serde_json::json!({ "path": "/work/b.rs", "content": "x" }),
                success: true,
                summary: "written".into(),
            },
        ];
        let evidence = SubagentEvidence::from_execution_log(&log);
        assert_eq!(evidence.files_accessed, vec!["/work/a.rs", "/work/b.rs"]);
        assert_eq!(evidence.tools_executed, vec!["read_file", "write_file"]);
    }

    #[test]
    fn citation_extraction_parses_markers_in_order_and_deduplicates() {
        let text =
            "read [file:/work/a.rs] then [tool:read_file] again [file:/work/a.rs] and [tool:grep]";
        let citations = SubagentCitations::extract(text);
        assert_eq!(citations.files, vec!["/work/a.rs"]);
        assert_eq!(citations.tools, vec!["read_file", "grep"]);
        assert!(!citations.is_empty());
    }

    #[test]
    fn citation_extraction_ignores_malformed_markers() {
        assert_eq!(SubagentCitations::extract("no markers").files.len(), 0);
        assert_eq!(SubagentCitations::extract("[file:unterminated").files.len(), 0);
        assert_eq!(SubagentCitations::extract("[file:]").files.len(), 0, "empty tokens skipped");
        assert_eq!(SubagentCitations::extract("[tool: read_file ]").tools, vec!["read_file"]);
    }

    #[test]
    fn valid_cited_summary_verifies_and_merges() {
        let mut memory = MemoryStore::new(1024);
        let item = MemoryItem::from_task("fact", "value", crate::tasks::TaskId::new("task-1"));
        let result = completed_result(
            "found [file:/work/a.rs] [tool:read_file]",
            SubagentCitations { files: vec!["/work/a.rs".into()], tools: vec!["read_file".into()] },
            vec![item.clone()],
        );
        let verifier = SubagentResultVerifier::new();
        let verification =
            verifier.verify_and_merge(&mut memory, &researcher(), &result, &evidence());
        assert!(verification.verified, "{:?}", verification);
        assert_eq!(memory.query("fact"), Some(item), "verified child memory merges");
    }

    #[test]
    fn missing_citation_rejects_merge_and_lists_what_is_missing() {
        let mut memory = MemoryStore::new(1024);
        let result = completed_result(
            "saw [file:/etc/passwd] and [tool:curl]",
            SubagentCitations { files: vec!["/etc/passwd".into()], tools: vec!["curl".into()] },
            vec![MemoryItem::from_task("fact", "value", crate::tasks::TaskId::new("task-1"))],
        );
        let verifier = SubagentResultVerifier::new();
        let verification =
            verifier.verify_and_merge(&mut memory, &researcher(), &result, &evidence());
        assert!(!verification.verified);
        assert_eq!(
            verification.missing_citations,
            vec!["file:/etc/passwd".to_string(), "tool:curl".to_string()]
        );
        assert!(memory.is_empty(), "rejected child memory never merges");
    }

    #[test]
    fn evidence_requiring_role_without_citations_is_rejected() {
        let result =
            completed_result("answer without claims", SubagentCitations::default(), Vec::new());
        let verification =
            SubagentResultVerifier::new().verify(&researcher(), &result, &evidence());
        assert!(!verification.verified);
        assert!(verification.rejected_reason.unwrap().contains("no cited files or tools"));
    }

    #[test]
    fn summarizer_role_merges_without_evidence() {
        let mut memory = MemoryStore::new(1024);
        let result = completed_result("summary", SubagentCitations::default(), Vec::new());
        let verification = SubagentResultVerifier::new().verify_and_merge(
            &mut memory,
            &summarizer(),
            &result,
            &evidence(),
        );
        assert!(verification.verified, "summarizer needs no citations");
    }

    #[test]
    fn non_completed_results_are_never_verified() {
        let mut result = completed_result("x", SubagentCitations::default(), Vec::new());
        result.handoff.status = SubagentStatus::Failed;
        let verification =
            SubagentResultVerifier::new().verify(&researcher(), &result, &evidence());
        assert!(!verification.verified);
        assert!(verification.rejected_reason.unwrap().contains("did not complete"));
    }

    #[test]
    fn citation_caps_reject_oversized_summaries() {
        let files: Vec<String> = (0..10).map(|index| format!("/work/f{index}.rs")).collect();
        let result =
            completed_result("lots", SubagentCitations { files, tools: Vec::new() }, Vec::new());
        let verifier = SubagentResultVerifier::with_limits(5, 5);
        let verification = verifier.verify(&researcher(), &result, &evidence());
        assert!(!verification.verified);
        assert!(verification.rejected_reason.unwrap().contains("too many cited files"));
    }

    #[test]
    fn quarantine_stores_failed_output_bounded_and_in_stable_order() {
        let mut quarantine = SubagentQuarantine::default();
        let long = "x".repeat(SUBAGENT_SUMMARY_MAX_CHARS + 100);
        let failed = SubagentResult {
            subagent_id: SubagentId::new("task-2"),
            handoff: SubagentHandoff::terminal("worker", "task-2", SubagentStatus::Failed),
            produced_memory_items: vec![MemoryItem::new("fact", "value")],
            tool_call_count: 1,
            error_summary: Some(long.clone()),
        };
        quarantine.quarantine(&failed, "subagent failed");
        let cancelled = SubagentResult {
            subagent_id: SubagentId::new("task-1"),
            handoff: SubagentHandoff::terminal("worker", "task-1", SubagentStatus::Cancelled),
            produced_memory_items: Vec::new(),
            tool_call_count: 0,
            error_summary: Some("cancelled".into()),
        };
        quarantine.quarantine(&cancelled, "subagent cancelled");

        assert_eq!(quarantine.len(), 2);
        assert!(quarantine.is_quarantined(&SubagentId::new("task-1")));
        assert!(quarantine.is_quarantined(&SubagentId::new("task-2")));
        let ids: Vec<String> = quarantine
            .entries()
            .iter()
            .map(|entry| entry.subagent_id.as_str().to_string())
            .collect();
        assert_eq!(ids, vec!["task-1", "task-2"], "stable subagent-id order");
        let entry = quarantine.entry(&SubagentId::new("task-2")).expect("entry");
        assert_eq!(entry.status, SubagentStatus::Failed);
        assert_eq!(entry.reason, "subagent failed");
        assert!(
            entry.error_summary.as_ref().expect("error").chars().count()
                <= SUBAGENT_SUMMARY_MAX_CHARS + 1
        );
        assert!(!entry.error_summary.as_ref().expect("error").contains(&long));
    }

    #[test]
    fn quarantined_output_is_excluded_from_memory_context() {
        let memory = MemoryStore::new(1024);
        let mut quarantine = SubagentQuarantine::default();
        let failed = SubagentResult {
            subagent_id: SubagentId::new("task-2"),
            handoff: SubagentHandoff::terminal("worker", "task-2", SubagentStatus::Failed),
            produced_memory_items: vec![MemoryItem::new("secret_fact", "leaked")],
            tool_call_count: 0,
            error_summary: Some("boom".into()),
        };
        quarantine.quarantine(&failed, "subagent failed");
        let context = memory.compact_context();
        assert!(context.is_none(), "quarantined output never reaches memory context");
        assert!(quarantine.quarantine_summary(4096).expect("summary").contains("boom"));
        let summary = quarantine.quarantine_summary(10).expect("bounded summary");
        assert!(summary.chars().count() <= 11, "summary is bounded");
    }

    #[test]
    fn quarantine_summary_is_none_when_empty_and_clear_resets() {
        let mut quarantine = SubagentQuarantine::default();
        assert_eq!(quarantine.quarantine_summary(1024), None);
        let failed = SubagentResult {
            subagent_id: SubagentId::new("task-2"),
            handoff: SubagentHandoff::terminal("worker", "task-2", SubagentStatus::Failed),
            produced_memory_items: Vec::new(),
            tool_call_count: 0,
            error_summary: Some("boom".into()),
        };
        quarantine.quarantine(&failed, "subagent failed");
        quarantine.clear();
        assert!(quarantine.is_empty());
        assert!(!quarantine.is_quarantined(&SubagentId::new("task-2")));
    }

    #[test]
    fn verification_and_quarantine_roundtrip_through_json() {
        let verification = SubagentVerification::rejected("missing citations: file:/x");
        let json = serde_json::to_string(&verification).expect("serializes");
        let restored: SubagentVerification = serde_json::from_str(&json).expect("parses");
        assert_eq!(restored, verification);

        let mut quarantine = SubagentQuarantine::default();
        quarantine.quarantine(
            &SubagentResult {
                subagent_id: SubagentId::new("task-2"),
                handoff: SubagentHandoff::terminal("worker", "task-2", SubagentStatus::Cancelled),
                produced_memory_items: Vec::new(),
                tool_call_count: 0,
                error_summary: Some("cancelled".into()),
            },
            "subagent cancelled",
        );
        let json = serde_json::to_string(&quarantine).expect("serializes");
        let restored: SubagentQuarantine = serde_json::from_str(&json).expect("parses");
        assert_eq!(restored, quarantine);
    }
}
