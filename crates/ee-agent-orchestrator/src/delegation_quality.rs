//! Subagent delegation quality controls.
//!
//! This module keeps delegation a root-agent decision. Before a child is
//! spawned, [`DelegationPreflight`] deterministically evaluates information
//! gain, token/cost limits, recursion, duplicate work, and intended write
//! ownership. Parallel requests are accepted only when their work keys and
//! absolute write scopes are independent. Child reports use a role-neutral,
//! evidence-carrying schema; verifier roles reject uncited claims and retain
//! the distinction between observed facts and inference. Root synthesis must
//! explicitly resolve contradictory findings before it can be selected.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// Version of the delegation governance data contract.
pub const DELEGATION_QUALITY_SCHEMA_VERSION: u32 = 1;

/// Fixed-point confidence, avoiding platform-dependent float comparisons.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FindingConfidence {
    Low,
    Medium,
    High,
}

/// Whether a finding reports observed evidence or a conclusion inferred from it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FindingKind {
    Observed,
    Inference,
}

/// One cited file or tool supporting a subagent finding.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FindingEvidence {
    File(String),
    Tool(String),
}

impl FindingEvidence {
    fn label(&self) -> String {
        match self {
            Self::File(path) => format!("file:{path}"),
            Self::Tool(name) => format!("tool:{name}"),
        }
    }
}

/// One actionable subagent finding.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct SubagentFinding {
    /// Stable topic used to reconcile independently reported findings.
    pub key: String,
    /// Narrow claim, never hidden reasoning.
    pub claim: String,
    /// Whether this claim is directly observed or inferred from evidence.
    pub kind: FindingKind,
    /// Evidence citations observed during the child run.
    pub evidence: Vec<FindingEvidence>,
    /// Child's calibrated confidence.
    pub confidence: FindingConfidence,
    /// Plausible alternatives considered and rejected with evidence.
    pub rejected_alternatives: Vec<String>,
    /// Safe next action for the root agent.
    pub recommended_next_action: String,
}

/// Structured child response required from verifier-capable roles.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct SubagentReport {
    /// Schema version for compatibility checks.
    pub schema_version: u32,
    /// Child role name.
    pub role: String,
    /// Stable child/task identifier.
    pub subagent_id: String,
    /// Findings in response order.
    pub findings: Vec<SubagentFinding>,
}

impl SubagentReport {
    /// Creates an empty versioned report for one child.
    #[must_use]
    pub fn new(role: impl Into<String>, subagent_id: impl Into<String>) -> Self {
        Self {
            schema_version: DELEGATION_QUALITY_SCHEMA_VERSION,
            role: role.into(),
            subagent_id: subagent_id.into(),
            findings: Vec::new(),
        }
    }
}

/// Evidence observed while one child executed.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct ReportEvidence {
    /// File paths accessed by the child.
    pub files: BTreeSet<String>,
    /// Tool names executed by the child.
    pub tools: BTreeSet<String>,
}

impl ReportEvidence {
    /// Builds evidence from the existing subagent execution projection.
    #[must_use]
    pub fn from_subagent_evidence(evidence: &crate::subagent_verifier::SubagentEvidence) -> Self {
        Self {
            files: evidence.files_accessed.iter().cloned().collect(),
            tools: evidence.tools_executed.iter().cloned().collect(),
        }
    }

    fn contains(&self, citation: &FindingEvidence) -> bool {
        match citation {
            FindingEvidence::File(path) => self.files.contains(path),
            FindingEvidence::Tool(name) => self.tools.contains(name),
        }
    }
}

/// Result of report-schema verification.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct ReportVerification {
    /// Whether root may use this report for synthesis.
    pub accepted: bool,
    /// Deterministic rejection reasons, in finding order.
    pub rejected_reasons: Vec<String>,
}

/// A report accepted by [`SubagentReportVerifier`]. Raw child reports cannot
/// enter [`reconcile_reports`] without this proof.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VerifiedSubagentReport {
    report: SubagentReport,
}

impl VerifiedSubagentReport {
    /// The report whose citations and schema were accepted.
    #[must_use]
    pub fn report(&self) -> &SubagentReport {
        &self.report
    }
}

/// Verifies role reports before they reach root-agent synthesis.
#[derive(Debug, Clone, Copy, Default)]
pub struct SubagentReportVerifier;

impl SubagentReportVerifier {
    /// Rejects malformed reports, uncited claims, citations absent from
    /// observed execution, and reports that blur observation with inference.
    #[must_use]
    pub fn verify(&self, report: &SubagentReport, observed: &ReportEvidence) -> ReportVerification {
        let mut rejected_reasons = Vec::new();
        if report.schema_version != DELEGATION_QUALITY_SCHEMA_VERSION {
            rejected_reasons
                .push(format!("unsupported report schema version {}", report.schema_version));
        }
        if report.role.trim().is_empty() || report.subagent_id.trim().is_empty() {
            rejected_reasons.push("report requires role and subagent id".into());
        }
        for finding in &report.findings {
            if finding.key.trim().is_empty() || finding.claim.trim().is_empty() {
                rejected_reasons.push("finding requires non-empty key and claim".into());
            }
            if finding.evidence.is_empty() {
                rejected_reasons.push(format!("uncited claim for {}", finding.key));
            }
            if finding.recommended_next_action.trim().is_empty() {
                rejected_reasons
                    .push(format!("finding {} has no recommended next action", finding.key));
            }
            for citation in &finding.evidence {
                if !observed.contains(citation) {
                    rejected_reasons.push(format!(
                        "unsupported citation {} for {}",
                        citation.label(),
                        finding.key
                    ));
                }
            }
        }
        ReportVerification { accepted: rejected_reasons.is_empty(), rejected_reasons }
    }

    /// Verifies `report` and returns a proof-carrying report only on success.
    /// Root synthesis accepts this type rather than raw model-produced output.
    pub fn verify_and_accept(
        &self,
        report: SubagentReport,
        observed: &ReportEvidence,
    ) -> Result<VerifiedSubagentReport, ReportVerification> {
        let verification = self.verify(&report, observed);
        if verification.accepted {
            Ok(VerifiedSubagentReport { report })
        } else {
            Err(verification)
        }
    }
}

/// Severity of overlap risk estimated before child dispatch.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WriteConflictRisk {
    None,
    Potential,
    Definite,
}

/// Cost and value estimate attached to a proposed delegation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct DelegationEstimate {
    /// Expected useful findings, capped by root policy rather than model text.
    pub expected_information_gain: u32,
    /// Expected input tokens for the child.
    pub input_tokens: u64,
    /// Expected output tokens for the child.
    pub output_tokens: u64,
    /// Estimated integer micro-USD cost.
    pub estimated_cost_microusd: u64,
    /// Predicted write collision risk.
    pub write_conflict_risk: WriteConflictRisk,
}

/// A root-owned candidate for delegation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct DelegationProposal {
    /// Stable request id.
    pub id: String,
    /// Role that will receive the task.
    pub role: String,
    /// Work identity. Same key means duplicate work and cannot run in parallel.
    pub work_key: String,
    /// Delegate depth at which this child would run.
    pub depth: usize,
    /// Absolute paths or module directories a child may write; empty is read-only.
    pub write_scope: Vec<PathBuf>,
    /// Root-provided expected value/cost estimate.
    pub estimate: DelegationEstimate,
}

/// Root policy limits applied before any child is spawned.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct DelegationBudget {
    /// Maximum children accepted in this fan-out.
    pub max_subagents: usize,
    /// Maximum permitted child nesting depth.
    pub max_depth: usize,
    /// Maximum aggregate estimated input tokens.
    pub max_input_tokens: u64,
    /// Maximum aggregate estimated output tokens.
    pub max_output_tokens: u64,
    /// Maximum aggregate estimated cost in integer micro-USD.
    pub max_cost_microusd: u64,
    /// Minimum expected useful findings required to justify coordination cost.
    pub min_information_gain: u32,
}

impl Default for DelegationBudget {
    fn default() -> Self {
        Self {
            max_subagents: 4,
            max_depth: 2,
            max_input_tokens: 16_000,
            max_output_tokens: 8_000,
            max_cost_microusd: u64::MAX,
            min_information_gain: 1,
        }
    }
}

/// Preflight output: accepted proposals keep deterministic request order;
/// rejected proposals carry a root-visible reason and never reach dispatch.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct DelegationPreflightResult {
    pub accepted: Vec<DelegationProposal>,
    pub rejected: Vec<RejectedDelegation>,
}

/// One proposal rejected before delegation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct RejectedDelegation {
    pub id: String,
    pub reason: String,
}

/// Deterministic root-side preflight evaluator.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DelegationPreflight {
    budget: DelegationBudget,
}

impl DelegationPreflight {
    /// Builds a preflight evaluator from explicit root limits.
    #[must_use]
    pub fn new(budget: DelegationBudget) -> Self {
        Self { budget }
    }

    /// Evaluates proposals in input order. It rejects duplicate work keys,
    /// non-absolute scope paths, overlap with already accepted writes, low
    /// expected gain, recursion excess, and aggregate budget excess.
    #[must_use]
    pub fn assess(&self, proposals: Vec<DelegationProposal>) -> DelegationPreflightResult {
        let mut accepted = Vec::new();
        let mut rejected = Vec::new();
        let mut ids = BTreeSet::new();
        let mut work_keys = BTreeSet::new();
        let mut owners: BTreeMap<PathBuf, String> = BTreeMap::new();
        let mut input_tokens = 0u64;
        let mut output_tokens = 0u64;
        let mut cost = 0u64;

        for proposal in proposals {
            let reason = self.reject_reason(
                &proposal,
                &mut ids,
                &mut work_keys,
                &owners,
                accepted.len(),
                input_tokens,
                output_tokens,
                cost,
            );
            if let Some(reason) = reason {
                rejected.push(RejectedDelegation { id: proposal.id, reason });
                continue;
            }
            for path in &proposal.write_scope {
                owners.insert(path.clone(), proposal.id.clone());
            }
            input_tokens = input_tokens.saturating_add(proposal.estimate.input_tokens);
            output_tokens = output_tokens.saturating_add(proposal.estimate.output_tokens);
            cost = cost.saturating_add(proposal.estimate.estimated_cost_microusd);
            accepted.push(proposal);
        }
        DelegationPreflightResult { accepted, rejected }
    }

    #[allow(clippy::too_many_arguments)]
    fn reject_reason(
        &self,
        proposal: &DelegationProposal,
        ids: &mut BTreeSet<String>,
        work_keys: &mut BTreeSet<String>,
        owners: &BTreeMap<PathBuf, String>,
        accepted_count: usize,
        input_tokens: u64,
        output_tokens: u64,
        cost: u64,
    ) -> Option<String> {
        if proposal.id.trim().is_empty()
            || proposal.role.trim().is_empty()
            || proposal.work_key.trim().is_empty()
        {
            return Some("delegation requires id, role, and work key".into());
        }
        if !ids.insert(proposal.id.clone()) {
            return Some("duplicate delegation id".into());
        }
        if !work_keys.insert(proposal.work_key.clone()) {
            return Some("duplicate work key; work is not independent".into());
        }
        if proposal.depth > self.budget.max_depth {
            return Some(format!(
                "delegate depth {} exceeds max {}",
                proposal.depth, self.budget.max_depth
            ));
        }
        if accepted_count >= self.budget.max_subagents {
            return Some(format!("subagent count exceeds max {}", self.budget.max_subagents));
        }
        if proposal.estimate.expected_information_gain < self.budget.min_information_gain {
            return Some("expected information gain does not justify coordination cost".into());
        }
        if proposal.estimate.write_conflict_risk == WriteConflictRisk::Definite {
            return Some("estimated write-conflict risk is definite".into());
        }
        if let Some(path) = proposal.write_scope.iter().find(|path| !path.is_absolute()) {
            return Some(format!("write scope must be absolute: {}", path.display()));
        }
        for path in &proposal.write_scope {
            if let Some(owner) =
                owners.iter().find_map(|(held, owner)| paths_overlap(held, path).then_some(owner))
            {
                return Some(format!("write scope overlaps root-owned proposal {owner}"));
            }
        }
        if input_tokens.saturating_add(proposal.estimate.input_tokens)
            > self.budget.max_input_tokens
        {
            return Some("aggregate input token budget exceeded".into());
        }
        if output_tokens.saturating_add(proposal.estimate.output_tokens)
            > self.budget.max_output_tokens
        {
            return Some("aggregate output token budget exceeded".into());
        }
        if cost.saturating_add(proposal.estimate.estimated_cost_microusd)
            > self.budget.max_cost_microusd
        {
            return Some("aggregate cost budget exceeded".into());
        }
        None
    }
}

fn paths_overlap(left: &Path, right: &Path) -> bool {
    left.starts_with(right) || right.starts_with(left)
}

/// Root decision resolving one disagreement. No automatic conflict winner exists.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct RootResolution {
    /// Finding key with contradictory claims.
    pub key: String,
    /// Claim selected by the root agent after inspecting cited evidence.
    pub selected_claim: String,
    /// Short evidence-based reason, not chain-of-thought.
    pub reason: String,
}

/// Root-visible state of a finding group.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReconciliationState {
    Agreed,
    Resolved,
    Contradictory,
}

/// One reconciled finding, including supporting child reports.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct ReconciledFinding {
    pub key: String,
    pub state: ReconciliationState,
    pub claim: Option<String>,
    pub supporting_subagent_ids: Vec<String>,
    pub resolution_reason: Option<String>,
}

/// Root-only synthesis output. `ready_for_plan` is false until every
/// contradictory group receives an explicit evidence-based resolution.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct RootSynthesis {
    pub findings: Vec<ReconciledFinding>,
    pub ready_for_plan: bool,
}

/// Reconciles verified reports. Findings sharing a key agree only when they
/// have exactly one distinct claim; otherwise an explicit root resolution is
/// required before a plan or write decision can consume this synthesis.
#[must_use]
pub fn reconcile_reports(
    reports: &[VerifiedSubagentReport],
    resolutions: &[RootResolution],
) -> RootSynthesis {
    let mut grouped: BTreeMap<String, Vec<(&str, &str)>> = BTreeMap::new();
    for verified in reports {
        let report = verified.report();
        for finding in &report.findings {
            grouped
                .entry(finding.key.clone())
                .or_default()
                .push((&report.subagent_id, &finding.claim));
        }
    }
    let resolutions = resolutions
        .iter()
        .map(|resolution| (resolution.key.as_str(), resolution))
        .collect::<BTreeMap<_, _>>();
    let mut findings = Vec::new();
    let mut ready_for_plan = true;
    for (key, values) in grouped {
        let claims = values.iter().map(|(_, claim)| (*claim).to_string()).collect::<BTreeSet<_>>();
        let supporting_subagent_ids = values
            .iter()
            .map(|(id, _)| (*id).to_string())
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();
        if claims.len() == 1 {
            findings.push(ReconciledFinding {
                key,
                state: ReconciliationState::Agreed,
                claim: claims.into_iter().next(),
                supporting_subagent_ids,
                resolution_reason: None,
            });
            continue;
        }
        let Some(resolution) = resolutions.get(key.as_str()) else {
            ready_for_plan = false;
            findings.push(ReconciledFinding {
                key,
                state: ReconciliationState::Contradictory,
                claim: None,
                supporting_subagent_ids,
                resolution_reason: None,
            });
            continue;
        };
        if resolution.selected_claim.trim().is_empty() || resolution.reason.trim().is_empty() {
            ready_for_plan = false;
            findings.push(ReconciledFinding {
                key,
                state: ReconciliationState::Contradictory,
                claim: None,
                supporting_subagent_ids,
                resolution_reason: None,
            });
            continue;
        }
        findings.push(ReconciledFinding {
            key,
            state: ReconciliationState::Resolved,
            claim: Some(resolution.selected_claim.clone()),
            supporting_subagent_ids,
            resolution_reason: Some(resolution.reason.clone()),
        });
    }
    RootSynthesis { findings, ready_for_plan }
}

/// Counter-only delegation outcome recorded by replay/evaluation fixtures.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct DelegationEffectiveness {
    /// Role-keyed counts make quality changes attributable to delegation role.
    pub by_role: BTreeMap<String, RoleDelegationEffectiveness>,
}

/// Counter-only effectiveness metrics for one role.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct RoleDelegationEffectiveness {
    pub useful_findings: u64,
    pub duplicate_work: u64,
    pub write_conflicts: u64,
    pub latency_ms: u64,
    pub estimated_cost_microusd: u64,
}

/// Deterministic quality direction from recorded delegation metrics.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DelegationQualityImpact {
    Improved,
    Neutral,
    Degraded,
}

impl DelegationEffectiveness {
    /// Adds one redaction-safe, counter-only replay observation for `role`.
    pub fn record(
        &mut self,
        role: impl Into<String>,
        useful_findings: u64,
        duplicate_work: u64,
        write_conflicts: u64,
        latency_ms: u64,
        estimated_cost_microusd: u64,
    ) {
        let entry = self.by_role.entry(role.into()).or_default();
        entry.useful_findings = entry.useful_findings.saturating_add(useful_findings);
        entry.duplicate_work = entry.duplicate_work.saturating_add(duplicate_work);
        entry.write_conflicts = entry.write_conflicts.saturating_add(write_conflicts);
        entry.latency_ms = entry.latency_ms.saturating_add(latency_ms);
        entry.estimated_cost_microusd =
            entry.estimated_cost_microusd.saturating_add(estimated_cost_microusd);
    }

    /// Shows whether a role improved or degraded task quality from evidence.
    #[must_use]
    pub fn quality_impact(&self, role: &str) -> DelegationQualityImpact {
        let Some(metrics) = self.by_role.get(role) else {
            return DelegationQualityImpact::Neutral;
        };
        let waste = metrics.duplicate_work.saturating_add(metrics.write_conflicts);
        if metrics.useful_findings > waste {
            DelegationQualityImpact::Improved
        } else if metrics.useful_findings == 0 && waste == 0 {
            DelegationQualityImpact::Neutral
        } else {
            DelegationQualityImpact::Degraded
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn proposal(id: &str, work_key: &str, path: Option<&str>) -> DelegationProposal {
        DelegationProposal {
            id: id.into(),
            role: "researcher".into(),
            work_key: work_key.into(),
            depth: 1,
            write_scope: path.into_iter().map(PathBuf::from).collect(),
            estimate: DelegationEstimate {
                expected_information_gain: 2,
                input_tokens: 10,
                output_tokens: 5,
                estimated_cost_microusd: 20,
                write_conflict_risk: WriteConflictRisk::None,
            },
        }
    }

    #[test]
    fn preflight_accepts_independent_scopes_within_budget() {
        let result = DelegationPreflight::new(DelegationBudget::default()).assess(vec![
            proposal("one", "read-config", Some("/work/config.rs")),
            proposal("two", "read-parser", Some("/work/parser.rs")),
        ]);
        assert_eq!(result.accepted.len(), 2);
        assert!(result.rejected.is_empty());
    }

    #[test]
    fn preflight_rejects_duplicate_work_and_overlapping_write_owner() {
        let result = DelegationPreflight::new(DelegationBudget::default()).assess(vec![
            proposal("one", "change-config", Some("/work/src")),
            proposal("two", "change-config", Some("/work/src/config.rs")),
            proposal("three", "change-parser", Some("/work/src/parser.rs")),
        ]);
        assert_eq!(result.accepted.len(), 1);
        assert_eq!(result.rejected.len(), 2);
        assert!(result.rejected[0].reason.contains("duplicate work"));
        assert!(result.rejected[1].reason.contains("overlaps"));
    }

    #[test]
    fn preflight_rejects_low_gain_depth_and_budget_excess() {
        let budget = DelegationBudget { max_input_tokens: 10, ..DelegationBudget::default() };
        let mut low_gain = proposal("low", "low", None);
        low_gain.estimate.expected_information_gain = 0;
        let mut deep = proposal("deep", "deep", None);
        deep.depth = 3;
        let mut too_expensive = proposal("cost", "cost", None);
        too_expensive.estimate.input_tokens = 11;
        let result = DelegationPreflight::new(budget).assess(vec![low_gain, deep, too_expensive]);
        assert!(result.accepted.is_empty());
        assert_eq!(result.rejected.len(), 3);
        assert!(result.rejected[0].reason.contains("information gain"));
        assert!(result.rejected[1].reason.contains("depth"));
        assert!(result.rejected[2].reason.contains("input token"));
    }

    #[test]
    fn verifier_rejects_uncited_claims_and_unsupported_evidence() {
        let mut report = SubagentReport::new("reviewer", "child-1");
        report.findings.push(SubagentFinding {
            key: "auth".into(),
            claim: "token validation is missing".into(),
            kind: FindingKind::Inference,
            evidence: vec![FindingEvidence::File("/work/auth.rs".into())],
            confidence: FindingConfidence::High,
            rejected_alternatives: vec!["middleware validates it".into()],
            recommended_next_action: "inspect middleware".into(),
        });
        let verified = SubagentReportVerifier.verify(&report, &ReportEvidence::default());
        assert!(!verified.accepted);
        assert!(verified.rejected_reasons[0].contains("unsupported citation"));

        report.findings[0].evidence.clear();
        let verified = SubagentReportVerifier.verify(&report, &ReportEvidence::default());
        assert!(!verified.accepted);
        assert!(verified.rejected_reasons.iter().any(|reason| reason.contains("uncited claim")));
    }

    #[test]
    fn verifier_accepts_cited_observation_and_preserves_kind() {
        let mut report = SubagentReport::new("researcher", "child-1");
        report.findings.push(SubagentFinding {
            key: "parser".into(),
            claim: "parser returns an error".into(),
            kind: FindingKind::Observed,
            evidence: vec![FindingEvidence::Tool("read_file".into())],
            confidence: FindingConfidence::High,
            rejected_alternatives: Vec::new(),
            recommended_next_action: "review caller".into(),
        });
        let observed = ReportEvidence {
            files: BTreeSet::new(),
            tools: ["read_file".to_string()].into_iter().collect(),
        };
        let verified = SubagentReportVerifier.verify(&report, &observed);
        assert!(verified.accepted, "{verified:?}");
        assert_eq!(report.findings[0].kind, FindingKind::Observed);
    }

    #[test]
    fn contradictory_reports_require_root_resolution() {
        let mut first = SubagentReport::new("researcher", "child-1");
        first.findings.push(finding("config", "flag is enabled"));
        let mut second = SubagentReport::new("reviewer", "child-2");
        second.findings.push(finding("config", "flag is disabled"));

        let observed = ReportEvidence {
            files: BTreeSet::new(),
            tools: ["read_file".to_string()].into_iter().collect(),
        };
        let verifier = SubagentReportVerifier;
        let first =
            verifier.verify_and_accept(first, &observed).expect("first report has cited evidence");
        let second = verifier
            .verify_and_accept(second, &observed)
            .expect("second report has cited evidence");

        let unresolved = reconcile_reports(&[first.clone(), second.clone()], &[]);
        assert!(!unresolved.ready_for_plan);
        assert_eq!(unresolved.findings[0].state, ReconciliationState::Contradictory);

        let resolved = reconcile_reports(
            &[first, second],
            &[RootResolution {
                key: "config".into(),
                selected_claim: "flag is disabled".into(),
                reason: "child-2 cites current configuration".into(),
            }],
        );
        assert!(resolved.ready_for_plan);
        assert_eq!(resolved.findings[0].state, ReconciliationState::Resolved);
        assert_eq!(resolved.findings[0].supporting_subagent_ids, vec!["child-1", "child-2"]);
    }

    #[test]
    fn replay_metrics_expose_role_quality_direction() {
        let mut metrics = DelegationEffectiveness::default();
        metrics.record("researcher", 3, 0, 0, 15, 40);
        metrics.record("implementer", 1, 2, 1, 20, 60);
        assert_eq!(metrics.quality_impact("researcher"), DelegationQualityImpact::Improved);
        assert_eq!(metrics.quality_impact("implementer"), DelegationQualityImpact::Degraded);
        assert_eq!(metrics.quality_impact("unknown"), DelegationQualityImpact::Neutral);
    }

    fn finding(key: &str, claim: &str) -> SubagentFinding {
        SubagentFinding {
            key: key.into(),
            claim: claim.into(),
            kind: FindingKind::Inference,
            evidence: vec![FindingEvidence::Tool("read_file".into())],
            confidence: FindingConfidence::Medium,
            rejected_alternatives: Vec::new(),
            recommended_next_action: "root decides".into(),
        }
    }
}
