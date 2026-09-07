//! Deterministic CI replay evidence for rubber-duck critique.
//!
//! This test-utils module evaluates checked-in scripted evidence only. It does not measure
//! real-provider quality, contact providers, grant runtime permission, or change production
//! defaults. Passing evidence can support a later rollout decision; manual internal critique
//! remains production default.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

pub(crate) use serde::{Deserialize, Serialize};

pub(crate) use crate::completion::CompletionState;
pub(crate) use crate::critique::{
    CritiqueReport, CritiqueReportVerifier, CritiqueTarget, build_critique_messages,
};
pub(crate) use crate::delegation_quality::ReportEvidence;
pub(crate) use crate::evaluation::{
    FixtureRun, default_evaluation_profile, required_fixture_suite, run_fixture,
};
pub(crate) use crate::model::ModelRole;
pub(crate) use crate::prompt_injection::POLICY_REMINDER;
pub(crate) use crate::review_context::ReviewContext;
pub(crate) use crate::rubber_duck::FindingResolution;
pub(crate) use crate::rubber_duck_config::RubberDuckBackend;
pub(crate) use crate::rubber_duck_trigger::{
    RubberDuckTrigger, RubberDuckTriggerConfig, RubberDuckTriggerDecision, RubberDuckTriggerFacts,
    RubberDuckTriggerMode, RubberDuckTriggerPolicy, RubberDuckTriggerReason,
    RubberDuckTriggerSkipReason, WorkImpact,
};
pub(crate) use crate::strategy::{StrategyReason, TurnStrategy};
pub(crate) use crate::trust::TrustLevel;

/// Checked-in replay contract version.
pub const RUBBER_DUCK_EVALUATION_SCHEMA_VERSION: u32 = 1;
/// Checked-in critic fixture suite.
pub const REQUIRED_RUBBER_DUCK_FIXTURE_SUITE: &str =
    include_str!("../../tests/fixtures/replay/v1/rubber_duck_tasks.json");
/// Checked-in deterministic internal automatic-mode baseline.
pub const REQUIRED_RUBBER_DUCK_BASELINE: &str =
    include_str!("../../tests/fixtures/replay/v1/rubber_duck_baseline.json");
/// Manual internal critique remains production default.
pub const DEFAULT_RUBBER_DUCK_ROLLOUT: RubberDuckRollout = RubberDuckRollout::ManualInternal;
/// Required internal fixtures used by automatic-internal CI eligibility.
pub const REQUIRED_INTERNAL_FIXTURE_COUNT: u64 = 11;
/// Required external cross-agent attribution fixtures, excluded from internal gate metrics.
pub const REQUIRED_EXTERNAL_FIXTURE_COUNT: u64 = 4;

const MAX_FIXTURE_SUITE_BYTES: usize = 256 * 1024;
const MAX_BASELINE_BYTES: usize = 64 * 1024;
const MAX_FIXTURES: usize = 64;
const MAX_ID_CHARS: usize = 128;
const MAX_TEXT_CHARS: usize = 512;
const MAX_OBSERVED_EVIDENCE: usize = 64;
const MAX_FINDING_KEYS: usize = 16;
const MAX_RESOLUTIONS: usize = 16;
const MAX_REPOSITORY_CONTEXT_ITEMS: usize = 8;
const MAX_PLANNED_OR_CHANGED_FILES: usize = 1_024;
const MAX_REPEATED_FAILURES: usize = 1_024;
const MAX_CRITIC_CALLS: u64 = 1;
const MAX_CRITIC_LATENCY_MS: u64 = 60_000;
const MAX_CRITIC_TOKENS: u64 = 1_000_000;
const MAX_CRITIC_COST_MICROUSD: u64 = 10_000_000;
const MAX_CRITIC_COUNTER: u64 = 1_000;
const FALSE_POSITIVE_PENALTY: u64 = 25;

/// Policy pinned in code before automatic internal rollout eligibility is evaluated.
pub const PINNED_RUBBER_DUCK_GATE_THRESHOLDS: RubberDuckGateThresholds = RubberDuckGateThresholds {
    min_complex_quality_gain: 250,
    min_trivial_skip_percent: 90,
    max_false_positives: 1,
    max_duplicate_work: 0,
    max_policy_violations: 0,
    max_model_agent_calls: 32,
    max_latency_ms: 320,
    max_input_tokens: 850,
    max_output_tokens: 330,
    max_estimated_cost_microusd: 1_550,
    max_false_successes: 0,
    max_completion_regressions: 0,
};
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RubberDuckScenario {
    FlawedPlan,
    MissingAuthorization,
    StaleRevision,
    UnsafeMigration,
    MissingRegressionTests,
    CleanMechanicalEdit,
    FalsePositiveCritique,
    UnavailableCritic,
    Timeout,
    MalformedReport,
    RepositoryPromptInjection,
    CrossAgentSameImplementation,
    CrossAgentDifferentImplementation,
}

/// Privacy-safe backend attribution. Internal and external identities cannot be confused.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ReplayCriticBackend {
    Internal { provider_version: String, model_id: String, startup_model_id: String },
    External { agent_id: String, implementation: String, startup_model_id: String },
}

impl ReplayCriticBackend {
    /// Production backend route represented by this replay attribution.
    #[must_use]
    pub fn backend(&self) -> RubberDuckBackend {
        match self {
            Self::Internal { model_id, .. } => {
                RubberDuckBackend::InternalModel { model_id: model_id.clone() }
            }
            Self::External { agent_id, .. } => {
                RubberDuckBackend::ExternalAgent { agent_id: agent_id.clone() }
            }
        }
    }

    const fn is_internal(&self) -> bool {
        matches!(self, Self::Internal { .. })
    }
}

/// Strict owned projection of production trigger facts.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReplayTriggerFacts {
    pub trigger: RubberDuckTrigger,
    pub session_id: String,
    pub revision: String,
    pub material_fingerprint: String,
    pub strategy: TurnStrategy,
    pub strategy_reason: StrategyReason,
    pub impacts: BTreeSet<WorkImpact>,
    pub planned_file_count: usize,
    pub changed_file_count: usize,
    pub diagnostics_present: bool,
    pub validation_passed: bool,
    pub validation_partial_or_skipped: bool,
    pub recovery_occurred: bool,
    pub repeated_failure_count: usize,
    pub selected_adjacent_tests: bool,
    pub cancelled: bool,
}

/// Expected production trigger decision.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "decision", rename_all = "snake_case", deny_unknown_fields)]
pub enum ReplayTriggerExpectation {
    Run { reason: RubberDuckTriggerReason },
    Skip { reason: RubberDuckTriggerSkipReason },
}

/// Scripted critic terminal state. Reports still pass through production verifier.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case", deny_unknown_fields)]
pub enum ScriptedCriticOutcome {
    Completed { report: CritiqueReport },
    Unavailable { reason: String },
    Timeout { reason: String },
    Malformed { raw: String },
}

/// Counter-only critic usage and authority evidence.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ScriptedCriticCounters {
    pub calls: u64,
    pub latency_ms: u64,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub estimated_cost_microusd: u64,
    pub mutation_calls: u64,
    pub execute_calls: u64,
    pub delegate_calls: u64,
    pub approval_prompts: u64,
    pub policy_violations: u64,
}

impl ScriptedCriticCounters {
    fn all_zero(&self) -> bool {
        self.calls == 0
            && self.latency_ms == 0
            && self.input_tokens == 0
            && self.output_tokens == 0
            && self.estimated_cost_microusd == 0
            && self.mutation_calls == 0
            && self.execute_calls == 0
            && self.delegate_calls == 0
            && self.approval_prompts == 0
            && self.policy_violations == 0
    }

    fn usage_zero(&self) -> bool {
        self.calls == 0
            && self.latency_ms == 0
            && self.input_tokens == 0
            && self.output_tokens == 0
            && self.estimated_cost_microusd == 0
    }
}

/// Strict observed evidence converted to production verifier evidence at replay time.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReplayObservedEvidence {
    pub files: BTreeSet<String>,
    pub tools: BTreeSet<String>,
}

impl ReplayObservedEvidence {
    fn production_evidence(&self) -> ReportEvidence {
        ReportEvidence { files: self.files.clone(), tools: self.tools.clone() }
    }
}

/// Root-owned resolution bound to one verified finding key.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReplayFindingResolution {
    pub finding_key: String,
    pub resolution: FindingResolution,
}

/// Fixed fixture oracle. Quality labels are derived, never authored.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReplayOracle {
    pub expected_finding_keys: BTreeSet<String>,
    pub root_known_finding_keys: BTreeSet<String>,
    pub resolutions: Vec<ReplayFindingResolution>,
    pub complex: bool,
    pub trivial: bool,
}

/// One strict, bounded, hermetic replay fixture.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RubberDuckEvaluationFixture {
    pub schema_version: u32,
    pub id: String,
    pub scenario: RubberDuckScenario,
    pub root_fixture_id: String,
    pub backend: ReplayCriticBackend,
    pub target: CritiqueTarget,
    pub trigger: ReplayTriggerFacts,
    pub expected_trigger: ReplayTriggerExpectation,
    pub observed_evidence: ReplayObservedEvidence,
    pub repository_context: Vec<String>,
    pub critic_outcome: ScriptedCriticOutcome,
    pub critic_counters: ScriptedCriticCounters,
    pub oracle: ReplayOracle,
}

/// Verified critic terminal state retained by replay.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case", deny_unknown_fields)]
pub enum ReplayCriticTerminal {
    Completed { finding_keys: BTreeSet<String> },
    Skipped { reason: RubberDuckTriggerSkipReason },
    Unavailable,
    Timeout,
    Quarantined,
}

/// Metrics derived from root replay, verified findings, oracle, and keyed resolutions.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RubberDuckReplayMetrics {
    pub useful_findings: u64,
    pub missed_findings: u64,
    pub accepted_findings: u64,
    pub rejected_findings: u64,
    pub deferred_findings: u64,
    pub duplicate_work: u64,
    pub false_positives: u64,
    pub policy_violations: u64,
    pub model_agent_calls: u64,
    pub latency_ms: u64,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub estimated_cost_microusd: u64,
    pub root_quality_score: u8,
    pub final_quality_score: u8,
    pub host_validation_score: u8,
    pub final_validation_score: u8,
    pub quality_gain: i16,
    pub critic_mutation_calls: u64,
    pub critic_execute_calls: u64,
    pub critic_delegate_calls: u64,
    pub critic_approval_prompts: u64,
}

/// One root-harness plus critic-verifier replay result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RubberDuckReplayRun {
    pub fixture_id: String,
    pub scenario: RubberDuckScenario,
    pub backend: ReplayCriticBackend,
    pub root: FixtureRun,
    pub trigger: ReplayTriggerExpectation,
    pub critic_terminal: ReplayCriticTerminal,
    pub root_completion: CompletionState,
    pub final_completion: CompletionState,
    pub complex: bool,
    pub trivial: bool,
    pub metrics: RubberDuckReplayMetrics,
}

/// Stable serializable per-run CI record without full workspace trace.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RubberDuckReplaySummary {
    pub fixture_id: String,
    pub scenario: RubberDuckScenario,
    pub backend: ReplayCriticBackend,
    pub trigger: ReplayTriggerExpectation,
    pub critic_terminal: ReplayCriticTerminal,
    pub root_completion: CompletionState,
    pub final_completion: CompletionState,
    pub complex: bool,
    pub trivial: bool,
    pub metrics: RubberDuckReplayMetrics,
}

impl From<&RubberDuckReplayRun> for RubberDuckReplaySummary {
    fn from(run: &RubberDuckReplayRun) -> Self {
        Self {
            fixture_id: run.fixture_id.clone(),
            scenario: run.scenario,
            backend: run.backend.clone(),
            trigger: run.trigger.clone(),
            critic_terminal: run.critic_terminal.clone(),
            root_completion: run.root_completion,
            final_completion: run.final_completion,
            complex: run.complex,
            trivial: run.trivial,
            metrics: run.metrics.clone(),
        }
    }
}

/// Aggregate deterministic evidence. Quality/resource fields include internal fixtures only.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RubberDuckAggregate {
    pub fixture_count: u64,
    pub external_fixture_count: u64,
    pub complex_fixture_count: u64,
    pub complex_quality_gain: i64,
    pub trivial_fixture_count: u64,
    pub trivial_skip_count: u64,
    pub useful_findings: u64,
    pub missed_findings: u64,
    pub accepted_findings: u64,
    pub rejected_findings: u64,
    pub deferred_findings: u64,
    pub false_positives: u64,
    pub duplicate_work: u64,
    pub policy_violations: u64,
    pub model_agent_calls: u64,
    pub internal_model_agent_calls: u64,
    pub external_model_agent_calls: u64,
    pub latency_ms: u64,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub estimated_cost_microusd: u64,
    pub root_quality_score_sum: u64,
    pub final_quality_score_sum: u64,
    pub final_validation_score_sum: u64,
    pub critic_mutation_calls: u64,
    pub critic_execute_calls: u64,
    pub critic_delegate_calls: u64,
    pub critic_approval_prompts: u64,
    pub false_successes: u64,
    pub completion_regressions: u64,
}

/// Thresholds pinned before automatic internal critique becomes CI-eligible.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RubberDuckGateThresholds {
    pub min_complex_quality_gain: i64,
    pub min_trivial_skip_percent: u8,
    pub max_false_positives: u64,
    pub max_duplicate_work: u64,
    pub max_policy_violations: u64,
    pub max_model_agent_calls: u64,
    pub max_latency_ms: u64,
    pub max_input_tokens: u64,
    pub max_output_tokens: u64,
    pub max_estimated_cost_microusd: u64,
    pub max_false_successes: u64,
    pub max_completion_regressions: u64,
}

/// Checked-in internal aggregate and pinned policy copy.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RubberDuckEvaluationBaseline {
    pub schema_version: u32,
    pub aggregate: RubberDuckAggregate,
    pub thresholds: RubberDuckGateThresholds,
}

/// Directional baseline metric.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReplayMetric {
    UsefulFindings,
    MissedFindings,
    FalsePositives,
    DuplicateWork,
    PolicyViolations,
    ModelAgentCalls,
    LatencyMs,
    InputTokens,
    OutputTokens,
    EstimatedCostMicrousd,
    FinalQualityScoreSum,
    FinalValidationScoreSum,
    FalseSuccesses,
    CompletionRegressions,
}

/// Typed automatic-mode gate failure.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "class", rename_all = "snake_case", deny_unknown_fields)]
pub enum RubberDuckGateFailure {
    FixtureCount {
        expected: u64,
        actual: u64,
    },
    ComplexQualityGain {
        minimum: i64,
        actual: i64,
    },
    CriticAuthority {
        mutation_calls: u64,
        execute_calls: u64,
        delegate_calls: u64,
        approval_prompts: u64,
    },
    TrivialSkipRate {
        minimum_percent: u8,
        actual_percent: u8,
    },
    FalsePositives {
        maximum: u64,
        actual: u64,
    },
    PolicyViolations {
        maximum: u64,
        actual: u64,
    },
    ResourceBound {
        resource: ReplayResource,
        maximum: u64,
        actual: u64,
    },
    FalseSuccess {
        maximum: u64,
        actual: u64,
    },
    CompletionRegression {
        maximum: u64,
        actual: u64,
    },
    BaselineIncrease {
        metric: ReplayMetric,
        baseline: u64,
        actual: u64,
    },
    BaselineDecrease {
        metric: ReplayMetric,
        baseline: u64,
        actual: u64,
    },
}

/// Bounded aggregate resource dimension.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReplayResource {
    DuplicateWork,
    ModelAgentCalls,
    LatencyMs,
    InputTokens,
    OutputTokens,
    EstimatedCostMicrousd,
}

/// Complete deterministic gate report.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RubberDuckGateReport {
    pub passed: bool,
    pub aggregate: RubberDuckAggregate,
    pub failures: Vec<RubberDuckGateFailure>,
}

/// Rollout evidence state. Never changes runtime permission or production defaults.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RubberDuckRollout {
    ManualInternal,
    AutomaticInternalEligible,
    ExternalManualSandboxGateRequired,
}

/// Replay contract error.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RubberDuckEvaluationError(String);

impl fmt::Display for RubberDuckEvaluationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for RubberDuckEvaluationError {}

pub use aggregate::{aggregate_rubber_duck_runs, summarize_rubber_duck_runs};
pub use gate::{
    checked_in_rubber_duck_rollout_eligibility, evaluate_rubber_duck_gate,
    require_rubber_duck_gate_pass, rubber_duck_rollout_eligibility,
};
pub use run::{
    load_rubber_duck_baseline, load_rubber_duck_fixture_suite, required_rubber_duck_baseline,
    required_rubber_duck_fixture_suite, run_required_rubber_duck_suite, run_rubber_duck_fixture,
};

mod aggregate;
mod gate;
mod run;
mod validate;

#[cfg(test)]
mod tests;
