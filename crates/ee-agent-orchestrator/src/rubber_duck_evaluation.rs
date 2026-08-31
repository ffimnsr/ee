//! Deterministic CI replay evidence for rubber-duck critique.
//!
//! This test-utils module evaluates checked-in scripted evidence only. It does not measure
//! real-provider quality, contact providers, grant runtime permission, or change production
//! defaults. Passing evidence can support a later rollout decision; manual internal critique
//! remains production default.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use serde::{Deserialize, Serialize};

use crate::completion::CompletionState;
use crate::critique::{
    CritiqueReport, CritiqueReportVerifier, CritiqueTarget, build_critique_messages,
};
use crate::delegation_quality::ReportEvidence;
use crate::evaluation::{
    FixtureRun, default_evaluation_profile, required_fixture_suite, run_fixture,
};
use crate::model::ModelRole;
use crate::prompt_injection::POLICY_REMINDER;
use crate::review_context::ReviewContext;
use crate::rubber_duck::FindingResolution;
use crate::rubber_duck_config::RubberDuckBackend;
use crate::rubber_duck_trigger::{
    RubberDuckTrigger, RubberDuckTriggerConfig, RubberDuckTriggerDecision, RubberDuckTriggerFacts,
    RubberDuckTriggerMode, RubberDuckTriggerPolicy, RubberDuckTriggerReason,
    RubberDuckTriggerSkipReason, WorkImpact,
};
use crate::strategy::{StrategyReason, TurnStrategy};
use crate::trust::TrustLevel;

/// Checked-in replay contract version.
pub const RUBBER_DUCK_EVALUATION_SCHEMA_VERSION: u32 = 1;
/// Checked-in critic fixture suite.
pub const REQUIRED_RUBBER_DUCK_FIXTURE_SUITE: &str =
    include_str!("../tests/fixtures/replay/v1/rubber_duck_tasks.json");
/// Checked-in deterministic internal automatic-mode baseline.
pub const REQUIRED_RUBBER_DUCK_BASELINE: &str =
    include_str!("../tests/fixtures/replay/v1/rubber_duck_baseline.json");
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

/// Required critic scenario. Core variants occur once; cross-agent variants occur twice.
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

/// Parses and fully validates fixture JSON.
pub fn load_rubber_duck_fixture_suite(
    json: &str,
) -> Result<Vec<RubberDuckEvaluationFixture>, RubberDuckEvaluationError> {
    if json.len() > MAX_FIXTURE_SUITE_BYTES {
        return Err(error(format!(
            "rubber-duck fixture suite exceeds {MAX_FIXTURE_SUITE_BYTES} bytes"
        )));
    }
    let fixtures: Vec<RubberDuckEvaluationFixture> = serde_json::from_str(json)
        .map_err(|source| error(format!("invalid rubber-duck fixture JSON: {source}")))?;
    if fixtures.is_empty() || fixtures.len() > MAX_FIXTURES {
        return Err(error(format!(
            "rubber-duck fixture count must be between 1 and {MAX_FIXTURES}"
        )));
    }
    let roots = required_fixture_suite()
        .map_err(|source| error(format!("base fixture suite unavailable: {source}")))?;
    let root_ids = roots.iter().map(|fixture| fixture.id.as_str()).collect::<BTreeSet<_>>();
    let mut ids = BTreeSet::new();
    for fixture in &fixtures {
        validate_fixture(fixture, &root_ids)?;
        if !ids.insert(fixture.id.clone()) {
            return Err(error(format!("duplicate rubber-duck fixture id: {}", fixture.id)));
        }
    }
    Ok(fixtures)
}

/// Loads checked-in fixture suite and requires exact core coverage plus cross-agent pairs.
pub fn required_rubber_duck_fixture_suite()
-> Result<Vec<RubberDuckEvaluationFixture>, RubberDuckEvaluationError> {
    let fixtures = load_rubber_duck_fixture_suite(REQUIRED_RUBBER_DUCK_FIXTURE_SUITE)?;
    validate_required_coverage(&fixtures)?;
    Ok(fixtures)
}

/// Parses strict baseline and rejects policy weakening or nonsensical aggregate evidence.
pub fn load_rubber_duck_baseline(
    json: &str,
) -> Result<RubberDuckEvaluationBaseline, RubberDuckEvaluationError> {
    if json.len() > MAX_BASELINE_BYTES {
        return Err(error(format!("rubber-duck baseline exceeds {MAX_BASELINE_BYTES} bytes")));
    }
    let baseline: RubberDuckEvaluationBaseline = serde_json::from_str(json)
        .map_err(|source| error(format!("invalid rubber-duck baseline JSON: {source}")))?;
    if baseline.schema_version != RUBBER_DUCK_EVALUATION_SCHEMA_VERSION {
        return Err(error(format!(
            "unsupported rubber-duck baseline schema version {}",
            baseline.schema_version
        )));
    }
    if baseline.thresholds != PINNED_RUBBER_DUCK_GATE_THRESHOLDS {
        return Err(error("rubber-duck baseline thresholds differ from pinned Rust policy"));
    }
    validate_baseline_aggregate(&baseline.aggregate)?;
    let report = evaluate_rubber_duck_gate(&baseline, baseline.aggregate.clone());
    if !report.passed {
        return Err(error(format!("checked-in baseline fails pinned gate: {:?}", report.failures)));
    }
    Ok(baseline)
}

/// Loads strict checked-in gate baseline.
pub fn required_rubber_duck_baseline()
-> Result<RubberDuckEvaluationBaseline, RubberDuckEvaluationError> {
    load_rubber_duck_baseline(REQUIRED_RUBBER_DUCK_BASELINE)
}

/// Runs root fixture through existing harness, then evaluates scripted critic evidence.
pub async fn run_rubber_duck_fixture(
    fixture: &RubberDuckEvaluationFixture,
) -> Result<RubberDuckReplayRun, RubberDuckEvaluationError> {
    let roots = required_fixture_suite()
        .map_err(|source| error(format!("base fixture suite unavailable: {source}")))?;
    let root_ids = roots.iter().map(|root| root.id.as_str()).collect::<BTreeSet<_>>();
    validate_fixture(fixture, &root_ids)?;
    let root_fixture = roots
        .iter()
        .find(|root| root.id == fixture.root_fixture_id)
        .ok_or_else(|| error(format!("unknown root fixture: {}", fixture.root_fixture_id)))?;
    let root = run_fixture(root_fixture, default_evaluation_profile())
        .await
        .map_err(|source| error(format!("root fixture {} failed: {source}", fixture.id)))?;

    let trigger = evaluate_trigger(&fixture.trigger);
    if trigger != fixture.expected_trigger {
        return Err(error(format!(
            "fixture {} trigger changed: expected {:?}, got {:?}",
            fixture.id, fixture.expected_trigger, trigger
        )));
    }
    if fixture.scenario == RubberDuckScenario::RepositoryPromptInjection {
        verify_guarded_repository_context(fixture)?;
    }

    let critic_terminal = match &trigger {
        ReplayTriggerExpectation::Skip { reason } => {
            ReplayCriticTerminal::Skipped { reason: *reason }
        }
        ReplayTriggerExpectation::Run { .. } => verify_scripted_outcome(fixture)?,
    };
    let verified_keys = match &critic_terminal {
        ReplayCriticTerminal::Completed { finding_keys } => finding_keys.clone(),
        _ => BTreeSet::new(),
    };
    let resolution_map = bind_resolutions(fixture, &verified_keys)?;
    let useful_keys = verified_keys
        .intersection(&fixture.oracle.expected_finding_keys)
        .cloned()
        .collect::<BTreeSet<_>>();
    let false_positive_keys = verified_keys
        .difference(&fixture.oracle.expected_finding_keys)
        .cloned()
        .collect::<BTreeSet<_>>();
    let accepted_keys = resolution_keys(&resolution_map, ResolutionKind::Accepted);
    let rejected_keys = resolution_keys(&resolution_map, ResolutionKind::Rejected);
    let deferred_keys = resolution_keys(&resolution_map, ResolutionKind::Deferred);
    let addressed = fixture
        .oracle
        .root_known_finding_keys
        .union(&accepted_keys)
        .cloned()
        .collect::<BTreeSet<_>>();
    let root_quality_score = quality_score(
        &fixture.oracle.expected_finding_keys,
        &fixture.oracle.root_known_finding_keys,
        0,
    );
    let final_quality_score = quality_score(
        &fixture.oracle.expected_finding_keys,
        &addressed,
        false_positive_keys.len() as u64,
    );
    let root_completion = derive_root_completion(&root);
    let final_completion = root_completion;
    let host_validation_score = root.score.total;

    let metrics = RubberDuckReplayMetrics {
        useful_findings: useful_keys.len() as u64,
        missed_findings: fixture.oracle.expected_finding_keys.len() as u64
            - useful_keys.len() as u64,
        accepted_findings: accepted_keys.len() as u64,
        rejected_findings: rejected_keys.len() as u64,
        deferred_findings: deferred_keys.len() as u64,
        duplicate_work: accepted_keys.difference(&fixture.oracle.expected_finding_keys).count()
            as u64,
        false_positives: false_positive_keys.len() as u64,
        policy_violations: checked_add(
            root.score.policy_violations,
            fixture.critic_counters.policy_violations,
            "fixture policy violations",
        )?,
        model_agent_calls: checked_add(
            root.score.model_calls,
            fixture.critic_counters.calls,
            "fixture model/agent calls",
        )?,
        latency_ms: checked_add(
            root.score.latency_ms,
            fixture.critic_counters.latency_ms,
            "fixture latency",
        )?,
        input_tokens: checked_add(
            root.score.input_tokens,
            fixture.critic_counters.input_tokens,
            "fixture input tokens",
        )?,
        output_tokens: checked_add(
            root.score.output_tokens,
            fixture.critic_counters.output_tokens,
            "fixture output tokens",
        )?,
        estimated_cost_microusd: checked_add(
            root.score.estimated_cost_microusd,
            fixture.critic_counters.estimated_cost_microusd,
            "fixture estimated cost",
        )?,
        root_quality_score,
        final_quality_score,
        host_validation_score,
        final_validation_score: host_validation_score,
        quality_gain: i16::from(final_quality_score) - i16::from(root_quality_score),
        critic_mutation_calls: fixture.critic_counters.mutation_calls,
        critic_execute_calls: fixture.critic_counters.execute_calls,
        critic_delegate_calls: fixture.critic_counters.delegate_calls,
        critic_approval_prompts: fixture.critic_counters.approval_prompts,
    };
    Ok(RubberDuckReplayRun {
        fixture_id: fixture.id.clone(),
        scenario: fixture.scenario,
        backend: fixture.backend.clone(),
        root,
        trigger,
        critic_terminal,
        root_completion,
        final_completion,
        complex: fixture.oracle.complex,
        trivial: fixture.oracle.trivial,
        metrics,
    })
}

/// Runs checked-in suite in stable id order.
pub async fn run_required_rubber_duck_suite()
-> Result<Vec<RubberDuckReplayRun>, RubberDuckEvaluationError> {
    let mut fixtures = required_rubber_duck_fixture_suite()?;
    fixtures.sort_by(|left, right| left.id.cmp(&right.id));
    let mut runs = Vec::with_capacity(fixtures.len());
    for fixture in &fixtures {
        runs.push(run_rubber_duck_fixture(fixture).await?);
    }
    Ok(runs)
}

/// Produces stable per-run records for CI metric persistence.
#[must_use]
pub fn summarize_rubber_duck_runs(runs: &[RubberDuckReplayRun]) -> Vec<RubberDuckReplaySummary> {
    runs.iter().map(RubberDuckReplaySummary::from).collect()
}

/// Aggregates checked evidence with overflow detection. Internal gate metrics exclude external runs.
pub fn aggregate_rubber_duck_runs(
    runs: &[RubberDuckReplayRun],
) -> Result<RubberDuckAggregate, RubberDuckEvaluationError> {
    let mut aggregate = RubberDuckAggregate::default();
    for run in runs {
        if !run.backend.is_internal() {
            add(&mut aggregate.external_fixture_count, 1, "external fixture count")?;
            add(
                &mut aggregate.external_model_agent_calls,
                run.metrics.model_agent_calls,
                "external model/agent calls",
            )?;
            for (target, value, name) in [
                (
                    &mut aggregate.policy_violations,
                    run.metrics.policy_violations,
                    "external policy violations",
                ),
                (
                    &mut aggregate.critic_mutation_calls,
                    run.metrics.critic_mutation_calls,
                    "external critic mutation calls",
                ),
                (
                    &mut aggregate.critic_execute_calls,
                    run.metrics.critic_execute_calls,
                    "external critic execute calls",
                ),
                (
                    &mut aggregate.critic_delegate_calls,
                    run.metrics.critic_delegate_calls,
                    "external critic delegate calls",
                ),
                (
                    &mut aggregate.critic_approval_prompts,
                    run.metrics.critic_approval_prompts,
                    "external critic approval prompts",
                ),
            ] {
                add(target, value, name)?;
            }
            if completion_rank(run.final_completion) > completion_rank(run.root_completion) {
                add(&mut aggregate.false_successes, 1, "external false successes")?;
            }
            if completion_rank(run.final_completion) < completion_rank(run.root_completion) {
                add(&mut aggregate.completion_regressions, 1, "external completion regressions")?;
            }
            continue;
        }
        add(&mut aggregate.fixture_count, 1, "internal fixture count")?;
        add(
            &mut aggregate.internal_model_agent_calls,
            run.metrics.model_agent_calls,
            "internal model/agent calls",
        )?;
        if run.complex {
            add(&mut aggregate.complex_fixture_count, 1, "complex fixture count")?;
            aggregate.complex_quality_gain = aggregate
                .complex_quality_gain
                .checked_add(i64::from(run.metrics.quality_gain))
                .ok_or_else(|| error("complex quality gain overflow"))?;
        }
        if run.trivial {
            add(&mut aggregate.trivial_fixture_count, 1, "trivial fixture count")?;
            if matches!(run.critic_terminal, ReplayCriticTerminal::Skipped { .. }) {
                add(&mut aggregate.trivial_skip_count, 1, "trivial skip count")?;
            }
        }
        for (target, value, name) in [
            (&mut aggregate.useful_findings, run.metrics.useful_findings, "useful findings"),
            (&mut aggregate.missed_findings, run.metrics.missed_findings, "missed findings"),
            (&mut aggregate.accepted_findings, run.metrics.accepted_findings, "accepted findings"),
            (&mut aggregate.rejected_findings, run.metrics.rejected_findings, "rejected findings"),
            (&mut aggregate.deferred_findings, run.metrics.deferred_findings, "deferred findings"),
            (&mut aggregate.false_positives, run.metrics.false_positives, "false positives"),
            (&mut aggregate.duplicate_work, run.metrics.duplicate_work, "duplicate work"),
            (&mut aggregate.policy_violations, run.metrics.policy_violations, "policy violations"),
            (&mut aggregate.model_agent_calls, run.metrics.model_agent_calls, "model/agent calls"),
            (&mut aggregate.latency_ms, run.metrics.latency_ms, "latency"),
            (&mut aggregate.input_tokens, run.metrics.input_tokens, "input tokens"),
            (&mut aggregate.output_tokens, run.metrics.output_tokens, "output tokens"),
            (
                &mut aggregate.estimated_cost_microusd,
                run.metrics.estimated_cost_microusd,
                "estimated cost",
            ),
            (
                &mut aggregate.root_quality_score_sum,
                u64::from(run.metrics.root_quality_score),
                "root quality score",
            ),
            (
                &mut aggregate.final_quality_score_sum,
                u64::from(run.metrics.final_quality_score),
                "final quality score",
            ),
            (
                &mut aggregate.final_validation_score_sum,
                u64::from(run.metrics.final_validation_score),
                "final validation score",
            ),
            (
                &mut aggregate.critic_mutation_calls,
                run.metrics.critic_mutation_calls,
                "critic mutation calls",
            ),
            (
                &mut aggregate.critic_execute_calls,
                run.metrics.critic_execute_calls,
                "critic execute calls",
            ),
            (
                &mut aggregate.critic_delegate_calls,
                run.metrics.critic_delegate_calls,
                "critic delegate calls",
            ),
            (
                &mut aggregate.critic_approval_prompts,
                run.metrics.critic_approval_prompts,
                "critic approval prompts",
            ),
        ] {
            add(target, value, name)?;
        }
        if completion_rank(run.final_completion) > completion_rank(run.root_completion) {
            add(&mut aggregate.false_successes, 1, "false successes")?;
        }
        if completion_rank(run.final_completion) < completion_rank(run.root_completion) {
            add(&mut aggregate.completion_regressions, 1, "completion regressions")?;
        }
    }
    Ok(aggregate)
}

/// Applies pinned hard limits and directional checked-in baseline comparisons.
#[must_use]
pub fn evaluate_rubber_duck_gate(
    baseline: &RubberDuckEvaluationBaseline,
    aggregate: RubberDuckAggregate,
) -> RubberDuckGateReport {
    let thresholds = &PINNED_RUBBER_DUCK_GATE_THRESHOLDS;
    let mut failures = Vec::new();
    if aggregate.fixture_count != baseline.aggregate.fixture_count {
        failures.push(RubberDuckGateFailure::FixtureCount {
            expected: baseline.aggregate.fixture_count,
            actual: aggregate.fixture_count,
        });
    }
    let minimum_gain =
        thresholds.min_complex_quality_gain.max(baseline.aggregate.complex_quality_gain);
    if aggregate.complex_quality_gain < minimum_gain {
        failures.push(RubberDuckGateFailure::ComplexQualityGain {
            minimum: minimum_gain,
            actual: aggregate.complex_quality_gain,
        });
    }
    if aggregate.critic_mutation_calls != 0
        || aggregate.critic_execute_calls != 0
        || aggregate.critic_delegate_calls != 0
        || aggregate.critic_approval_prompts != 0
    {
        failures.push(RubberDuckGateFailure::CriticAuthority {
            mutation_calls: aggregate.critic_mutation_calls,
            execute_calls: aggregate.critic_execute_calls,
            delegate_calls: aggregate.critic_delegate_calls,
            approval_prompts: aggregate.critic_approval_prompts,
        });
    }
    let skip_percent = aggregate
        .trivial_skip_count
        .saturating_mul(100)
        .checked_div(aggregate.trivial_fixture_count)
        .unwrap_or(0) as u8;
    if skip_percent < thresholds.min_trivial_skip_percent {
        failures.push(RubberDuckGateFailure::TrivialSkipRate {
            minimum_percent: thresholds.min_trivial_skip_percent,
            actual_percent: skip_percent,
        });
    }
    if aggregate.false_positives > thresholds.max_false_positives {
        failures.push(RubberDuckGateFailure::FalsePositives {
            maximum: thresholds.max_false_positives,
            actual: aggregate.false_positives,
        });
    }
    if aggregate.policy_violations > thresholds.max_policy_violations {
        failures.push(RubberDuckGateFailure::PolicyViolations {
            maximum: thresholds.max_policy_violations,
            actual: aggregate.policy_violations,
        });
    }
    for (resource, actual, maximum) in [
        (ReplayResource::DuplicateWork, aggregate.duplicate_work, thresholds.max_duplicate_work),
        (
            ReplayResource::ModelAgentCalls,
            aggregate.model_agent_calls,
            thresholds.max_model_agent_calls,
        ),
        (ReplayResource::LatencyMs, aggregate.latency_ms, thresholds.max_latency_ms),
        (ReplayResource::InputTokens, aggregate.input_tokens, thresholds.max_input_tokens),
        (ReplayResource::OutputTokens, aggregate.output_tokens, thresholds.max_output_tokens),
        (
            ReplayResource::EstimatedCostMicrousd,
            aggregate.estimated_cost_microusd,
            thresholds.max_estimated_cost_microusd,
        ),
    ] {
        if actual > maximum {
            failures.push(RubberDuckGateFailure::ResourceBound { resource, maximum, actual });
        }
    }
    if aggregate.false_successes > thresholds.max_false_successes {
        failures.push(RubberDuckGateFailure::FalseSuccess {
            maximum: thresholds.max_false_successes,
            actual: aggregate.false_successes,
        });
    }
    if aggregate.completion_regressions > thresholds.max_completion_regressions {
        failures.push(RubberDuckGateFailure::CompletionRegression {
            maximum: thresholds.max_completion_regressions,
            actual: aggregate.completion_regressions,
        });
    }
    for (metric, actual, expected) in [
        (
            ReplayMetric::MissedFindings,
            aggregate.missed_findings,
            baseline.aggregate.missed_findings,
        ),
        (
            ReplayMetric::FalsePositives,
            aggregate.false_positives,
            baseline.aggregate.false_positives,
        ),
        (ReplayMetric::DuplicateWork, aggregate.duplicate_work, baseline.aggregate.duplicate_work),
        (
            ReplayMetric::PolicyViolations,
            aggregate.policy_violations,
            baseline.aggregate.policy_violations,
        ),
        (
            ReplayMetric::ModelAgentCalls,
            aggregate.model_agent_calls,
            baseline.aggregate.model_agent_calls,
        ),
        (ReplayMetric::LatencyMs, aggregate.latency_ms, baseline.aggregate.latency_ms),
        (ReplayMetric::InputTokens, aggregate.input_tokens, baseline.aggregate.input_tokens),
        (ReplayMetric::OutputTokens, aggregate.output_tokens, baseline.aggregate.output_tokens),
        (
            ReplayMetric::EstimatedCostMicrousd,
            aggregate.estimated_cost_microusd,
            baseline.aggregate.estimated_cost_microusd,
        ),
        (
            ReplayMetric::FalseSuccesses,
            aggregate.false_successes,
            baseline.aggregate.false_successes,
        ),
        (
            ReplayMetric::CompletionRegressions,
            aggregate.completion_regressions,
            baseline.aggregate.completion_regressions,
        ),
    ] {
        if actual > expected {
            failures.push(RubberDuckGateFailure::BaselineIncrease {
                metric,
                baseline: expected,
                actual,
            });
        }
    }
    for (metric, actual, expected) in [
        (
            ReplayMetric::UsefulFindings,
            aggregate.useful_findings,
            baseline.aggregate.useful_findings,
        ),
        (
            ReplayMetric::FinalQualityScoreSum,
            aggregate.final_quality_score_sum,
            baseline.aggregate.final_quality_score_sum,
        ),
        (
            ReplayMetric::FinalValidationScoreSum,
            aggregate.final_validation_score_sum,
            baseline.aggregate.final_validation_score_sum,
        ),
    ] {
        if actual < expected {
            failures.push(RubberDuckGateFailure::BaselineDecrease {
                metric,
                baseline: expected,
                actual,
            });
        }
    }
    RubberDuckGateReport { passed: failures.is_empty(), aggregate, failures }
}

/// Fails closed when automatic-mode gate has any failure.
pub fn require_rubber_duck_gate_pass(
    report: &RubberDuckGateReport,
) -> Result<(), RubberDuckEvaluationError> {
    if report.passed {
        Ok(())
    } else {
        Err(error(format!("rubber-duck deterministic replay gate failed: {:?}", report.failures)))
    }
}

/// Executes checked-in CI replay and returns evidence status. Does not grant runtime permission.
pub async fn checked_in_rubber_duck_rollout_eligibility(
    backend: &RubberDuckBackend,
) -> Result<(RubberDuckRollout, RubberDuckGateReport), RubberDuckEvaluationError> {
    let runs = run_required_rubber_duck_suite().await?;
    let baseline = required_rubber_duck_baseline()?;
    let aggregate = aggregate_rubber_duck_runs(&runs)?;
    let report = evaluate_rubber_duck_gate(&baseline, aggregate);
    let rollout = rubber_duck_rollout_eligibility(backend, &report);
    Ok((rollout, report))
}

/// Maps CI evidence to rollout status without changing production defaults or permissions.
#[must_use]
pub fn rubber_duck_rollout_eligibility(
    backend: &RubberDuckBackend,
    report: &RubberDuckGateReport,
) -> RubberDuckRollout {
    match backend {
        RubberDuckBackend::InternalModel { .. } if report.passed => {
            RubberDuckRollout::AutomaticInternalEligible
        }
        RubberDuckBackend::InternalModel { .. } => RubberDuckRollout::ManualInternal,
        RubberDuckBackend::ExternalAgent { .. } => {
            RubberDuckRollout::ExternalManualSandboxGateRequired
        }
    }
}

fn validate_fixture(
    fixture: &RubberDuckEvaluationFixture,
    root_ids: &BTreeSet<&str>,
) -> Result<(), RubberDuckEvaluationError> {
    if fixture.schema_version != RUBBER_DUCK_EVALUATION_SCHEMA_VERSION {
        return Err(error(format!(
            "fixture {} has unsupported schema version {}",
            fixture.id, fixture.schema_version
        )));
    }
    validate_id("fixture id", &fixture.id)?;
    validate_id("root fixture id", &fixture.root_fixture_id)?;
    if !root_ids.contains(fixture.root_fixture_id.as_str()) {
        return Err(error(format!(
            "fixture {} references missing root fixture {}",
            fixture.id, fixture.root_fixture_id
        )));
    }
    validate_id("session id", &fixture.trigger.session_id)?;
    if !fixture.trigger.revision.is_empty() {
        validate_id("revision", &fixture.trigger.revision)?;
    }
    validate_id("material fingerprint", &fixture.trigger.material_fingerprint)?;
    validate_backend(&fixture.backend)?;
    if fixture.trigger.planned_file_count > MAX_PLANNED_OR_CHANGED_FILES
        || fixture.trigger.changed_file_count > MAX_PLANNED_OR_CHANGED_FILES
        || fixture.trigger.repeated_failure_count > MAX_REPEATED_FAILURES
    {
        return Err(error(format!("fixture {} trigger numeric bound exceeded", fixture.id)));
    }
    if fixture.observed_evidence.files.len() + fixture.observed_evidence.tools.len()
        > MAX_OBSERVED_EVIDENCE
    {
        return Err(error(format!("fixture {} has too much observed evidence", fixture.id)));
    }
    for path in &fixture.observed_evidence.files {
        validate_hermetic_path(path)?;
    }
    for tool in &fixture.observed_evidence.tools {
        validate_bounded_text("observed tool", tool, MAX_ID_CHARS)?;
    }
    validate_oracle(fixture)?;
    validate_repository_context(fixture)?;
    validate_counters(&fixture.critic_counters)?;
    match &fixture.critic_outcome {
        ScriptedCriticOutcome::Completed { report } => {
            if report.target != fixture.target {
                return Err(error(format!("fixture {} critique target mismatch", fixture.id)));
            }
        }
        ScriptedCriticOutcome::Unavailable { reason }
        | ScriptedCriticOutcome::Timeout { reason } => {
            validate_bounded_text("scripted terminal reason", reason, MAX_TEXT_CHARS)?;
        }
        ScriptedCriticOutcome::Malformed { raw } => {
            validate_bounded_text("malformed output", raw, MAX_TEXT_CHARS)?;
        }
    }
    validate_terminal_counters(fixture)
}

fn validate_oracle(fixture: &RubberDuckEvaluationFixture) -> Result<(), RubberDuckEvaluationError> {
    let oracle = &fixture.oracle;
    if oracle.expected_finding_keys.len() > MAX_FINDING_KEYS
        || oracle.root_known_finding_keys.len() > MAX_FINDING_KEYS
        || oracle.resolutions.len() > MAX_RESOLUTIONS
    {
        return Err(error(format!("fixture {} oracle exceeds bounds", fixture.id)));
    }
    if !oracle.root_known_finding_keys.is_subset(&oracle.expected_finding_keys) {
        return Err(error(format!(
            "fixture {} root-known keys must be oracle finding keys",
            fixture.id
        )));
    }
    for key in oracle.expected_finding_keys.iter().chain(&oracle.root_known_finding_keys) {
        validate_id("oracle finding key", key)?;
    }
    let mut resolution_keys = BTreeSet::new();
    for resolution in &oracle.resolutions {
        validate_id("resolution finding key", &resolution.finding_key)?;
        validate_resolution(&resolution.resolution)?;
        if !resolution_keys.insert(&resolution.finding_key) {
            return Err(error(format!(
                "fixture {} has duplicate resolution key {}",
                fixture.id, resolution.finding_key
            )));
        }
    }
    if oracle.trivial && oracle.complex {
        return Err(error(format!("fixture {} cannot be complex and trivial", fixture.id)));
    }
    Ok(())
}

fn validate_repository_context(
    fixture: &RubberDuckEvaluationFixture,
) -> Result<(), RubberDuckEvaluationError> {
    if fixture.repository_context.len() > MAX_REPOSITORY_CONTEXT_ITEMS {
        return Err(error(format!("fixture {} repository context exceeds bounds", fixture.id)));
    }
    for item in &fixture.repository_context {
        validate_bounded_text("repository context", item, MAX_TEXT_CHARS)?;
    }
    if fixture.scenario == RubberDuckScenario::RepositoryPromptInjection {
        if fixture.repository_context.is_empty() {
            return Err(error("prompt-injection fixture requires repository context"));
        }
    } else if !fixture.repository_context.is_empty() {
        return Err(error(format!(
            "fixture {} carries repository context outside prompt-injection scenario",
            fixture.id
        )));
    }
    Ok(())
}

fn validate_counters(counters: &ScriptedCriticCounters) -> Result<(), RubberDuckEvaluationError> {
    for (name, actual, maximum) in [
        ("calls", counters.calls, MAX_CRITIC_CALLS),
        ("latency_ms", counters.latency_ms, MAX_CRITIC_LATENCY_MS),
        ("input_tokens", counters.input_tokens, MAX_CRITIC_TOKENS),
        ("output_tokens", counters.output_tokens, MAX_CRITIC_TOKENS),
        ("estimated_cost_microusd", counters.estimated_cost_microusd, MAX_CRITIC_COST_MICROUSD),
        ("mutation_calls", counters.mutation_calls, MAX_CRITIC_COUNTER),
        ("execute_calls", counters.execute_calls, MAX_CRITIC_COUNTER),
        ("delegate_calls", counters.delegate_calls, MAX_CRITIC_COUNTER),
        ("approval_prompts", counters.approval_prompts, MAX_CRITIC_COUNTER),
        ("policy_violations", counters.policy_violations, MAX_CRITIC_COUNTER),
    ] {
        if actual > maximum {
            return Err(error(format!("critic counter {name} exceeds {maximum}")));
        }
    }
    Ok(())
}

fn validate_terminal_counters(
    fixture: &RubberDuckEvaluationFixture,
) -> Result<(), RubberDuckEvaluationError> {
    let counters = &fixture.critic_counters;
    if matches!(fixture.expected_trigger, ReplayTriggerExpectation::Skip { .. }) {
        if !counters.all_zero() || !fixture.oracle.resolutions.is_empty() {
            return Err(error(format!(
                "skipped fixture {} must have zero critic counters and no resolutions",
                fixture.id
            )));
        }
        return Ok(());
    }
    match &fixture.critic_outcome {
        ScriptedCriticOutcome::Completed { .. } | ScriptedCriticOutcome::Malformed { .. } => {
            if counters.calls != 1
                || counters.latency_ms == 0
                || counters.input_tokens == 0
                || counters.output_tokens == 0
            {
                return Err(error(format!(
                    "completed/malformed fixture {} requires one call and nonzero usage",
                    fixture.id
                )));
            }
        }
        ScriptedCriticOutcome::Unavailable { .. } => {
            if !counters.usage_zero() || !fixture.oracle.resolutions.is_empty() {
                return Err(error(format!(
                    "unavailable fixture {} must have zero usage and no resolutions",
                    fixture.id
                )));
            }
        }
        ScriptedCriticOutcome::Timeout { .. } => {
            if counters.calls != 1
                || counters.latency_ms == 0
                || counters.input_tokens == 0
                || counters.output_tokens != 0
                || !fixture.oracle.resolutions.is_empty()
            {
                return Err(error(format!(
                    "timeout fixture {} has invalid call, usage, or resolution counters",
                    fixture.id
                )));
            }
        }
    }
    Ok(())
}

fn validate_required_coverage(
    fixtures: &[RubberDuckEvaluationFixture],
) -> Result<(), RubberDuckEvaluationError> {
    let mut counts = BTreeMap::new();
    for fixture in fixtures {
        *counts.entry(fixture.scenario).or_insert(0_usize) += 1;
    }
    for scenario in [
        RubberDuckScenario::FlawedPlan,
        RubberDuckScenario::MissingAuthorization,
        RubberDuckScenario::StaleRevision,
        RubberDuckScenario::UnsafeMigration,
        RubberDuckScenario::MissingRegressionTests,
        RubberDuckScenario::CleanMechanicalEdit,
        RubberDuckScenario::FalsePositiveCritique,
        RubberDuckScenario::UnavailableCritic,
        RubberDuckScenario::Timeout,
        RubberDuckScenario::MalformedReport,
        RubberDuckScenario::RepositoryPromptInjection,
    ] {
        if counts.get(&scenario) != Some(&1) {
            return Err(error(format!("required scenario must occur exactly once: {scenario:?}")));
        }
    }
    for scenario in [
        RubberDuckScenario::CrossAgentSameImplementation,
        RubberDuckScenario::CrossAgentDifferentImplementation,
    ] {
        if counts.get(&scenario) != Some(&2) {
            return Err(error(format!(
                "cross-agent scenario requires exactly two fixtures: {scenario:?}"
            )));
        }
    }
    let internal_count =
        fixtures.iter().filter(|fixture| fixture.backend.is_internal()).count() as u64;
    let external_count = fixtures.len() as u64 - internal_count;
    if internal_count != REQUIRED_INTERNAL_FIXTURE_COUNT
        || external_count != REQUIRED_EXTERNAL_FIXTURE_COUNT
    {
        return Err(error("required internal/external fixture counts changed"));
    }
    validate_cross_agent_attribution(fixtures)
}

fn validate_cross_agent_attribution(
    fixtures: &[RubberDuckEvaluationFixture],
) -> Result<(), RubberDuckEvaluationError> {
    let cross_agent_ids = fixtures
        .iter()
        .filter(|fixture| {
            matches!(
                fixture.scenario,
                RubberDuckScenario::CrossAgentSameImplementation
                    | RubberDuckScenario::CrossAgentDifferentImplementation
            )
        })
        .filter_map(|fixture| external_parts(fixture).map(|(agent_id, _, _)| agent_id))
        .collect::<BTreeSet<_>>();
    if cross_agent_ids.len() != REQUIRED_EXTERNAL_FIXTURE_COUNT as usize {
        return Err(error("cross-agent fixtures require globally distinct agent ids"));
    }
    for (scenario, same_implementation) in [
        (RubberDuckScenario::CrossAgentSameImplementation, true),
        (RubberDuckScenario::CrossAgentDifferentImplementation, false),
    ] {
        let pair =
            fixtures.iter().filter(|fixture| fixture.scenario == scenario).collect::<Vec<_>>();
        let Some((agent_a, implementation_a, model_a)) = external_parts(pair[0]) else {
            return Err(error("cross-agent fixture must use external backend"));
        };
        let Some((agent_b, implementation_b, model_b)) = external_parts(pair[1]) else {
            return Err(error("cross-agent fixture must use external backend"));
        };
        if agent_a == agent_b {
            return Err(error("cross-agent fixture pair requires distinct agent ids"));
        }
        if same_implementation {
            if implementation_a != implementation_b || model_a == model_b {
                return Err(error(
                    "same-implementation fixtures require equal implementation and distinct startup models",
                ));
            }
        } else if implementation_a == implementation_b {
            return Err(error("different ACP fixtures require distinct implementation identities"));
        }
    }
    Ok(())
}

fn external_parts(fixture: &RubberDuckEvaluationFixture) -> Option<(&str, &str, &str)> {
    match &fixture.backend {
        ReplayCriticBackend::External { agent_id, implementation, startup_model_id } => {
            Some((agent_id, implementation, startup_model_id))
        }
        ReplayCriticBackend::Internal { .. } => None,
    }
}

fn evaluate_trigger(facts: &ReplayTriggerFacts) -> ReplayTriggerExpectation {
    let borrowed = RubberDuckTriggerFacts {
        session_id: &facts.session_id,
        revision: &facts.revision,
        material_fingerprint: &facts.material_fingerprint,
        strategy: facts.strategy,
        strategy_reason: facts.strategy_reason,
        impacts: &facts.impacts,
        planned_file_count: facts.planned_file_count,
        changed_file_count: facts.changed_file_count,
        diagnostics_present: facts.diagnostics_present,
        validation_passed: facts.validation_passed,
        validation_partial_or_skipped: facts.validation_partial_or_skipped,
        recovery_occurred: facts.recovery_occurred,
        repeated_failure_count: facts.repeated_failure_count,
        selected_adjacent_tests: facts.selected_adjacent_tests,
        cancelled: facts.cancelled,
    };
    match RubberDuckTriggerPolicy::new(RubberDuckTriggerConfig {
        mode: RubberDuckTriggerMode::Automatic,
    })
    .evaluate(facts.trigger, &borrowed)
    {
        RubberDuckTriggerDecision::Run { reason, .. } => ReplayTriggerExpectation::Run { reason },
        RubberDuckTriggerDecision::Skip(reason) => ReplayTriggerExpectation::Skip { reason },
    }
}

fn verify_scripted_outcome(
    fixture: &RubberDuckEvaluationFixture,
) -> Result<ReplayCriticTerminal, RubberDuckEvaluationError> {
    match &fixture.critic_outcome {
        ScriptedCriticOutcome::Completed { report } => {
            let raw = serde_json::to_string(report).map_err(|source| {
                error(format!("fixture {} report encode failed: {source}", fixture.id))
            })?;
            let observed = fixture.observed_evidence.production_evidence();
            let verified = CritiqueReportVerifier
                .parse_and_accept_for_target(&raw, &fixture.target, &observed)
                .map_err(|source| {
                    error(format!("fixture {} report rejected: {source}", fixture.id))
                })?;
            Ok(ReplayCriticTerminal::Completed {
                finding_keys: verified
                    .report()
                    .findings
                    .iter()
                    .map(|finding| finding.key.clone())
                    .collect(),
            })
        }
        ScriptedCriticOutcome::Unavailable { .. } => Ok(ReplayCriticTerminal::Unavailable),
        ScriptedCriticOutcome::Timeout { .. } => Ok(ReplayCriticTerminal::Timeout),
        ScriptedCriticOutcome::Malformed { raw } => {
            if CritiqueReportVerifier.parse(raw).is_ok() {
                return Err(error(format!("fixture {} malformed report parsed", fixture.id)));
            }
            Ok(ReplayCriticTerminal::Quarantined)
        }
    }
}

fn bind_resolutions(
    fixture: &RubberDuckEvaluationFixture,
    verified_keys: &BTreeSet<String>,
) -> Result<BTreeMap<String, FindingResolution>, RubberDuckEvaluationError> {
    let mut resolutions = BTreeMap::new();
    for entry in &fixture.oracle.resolutions {
        if !verified_keys.contains(&entry.finding_key) {
            return Err(error(format!(
                "fixture {} resolution references unknown finding key {}",
                fixture.id, entry.finding_key
            )));
        }
        if resolutions.insert(entry.finding_key.clone(), entry.resolution.clone()).is_some() {
            return Err(error(format!(
                "fixture {} has duplicate resolution key {}",
                fixture.id, entry.finding_key
            )));
        }
    }
    let resolution_keys = resolutions.keys().cloned().collect::<BTreeSet<_>>();
    if &resolution_keys != verified_keys {
        let missing = verified_keys.difference(&resolution_keys).cloned().collect::<Vec<_>>();
        return Err(error(format!(
            "fixture {} missing resolutions for verified finding keys {missing:?}",
            fixture.id
        )));
    }
    Ok(resolutions)
}

#[derive(Clone, Copy)]
enum ResolutionKind {
    Accepted,
    Rejected,
    Deferred,
}

fn resolution_keys(
    resolutions: &BTreeMap<String, FindingResolution>,
    kind: ResolutionKind,
) -> BTreeSet<String> {
    resolutions
        .iter()
        .filter(|(_, resolution)| {
            matches!(
                (kind, resolution),
                (ResolutionKind::Accepted, FindingResolution::Accepted { .. })
                    | (ResolutionKind::Rejected, FindingResolution::Rejected { .. })
                    | (ResolutionKind::Deferred, FindingResolution::Deferred { .. })
            )
        })
        .map(|(key, _)| key.clone())
        .collect()
}

fn quality_score(
    expected: &BTreeSet<String>,
    addressed: &BTreeSet<String>,
    false_positives: u64,
) -> u8 {
    let recall = if expected.is_empty() {
        100
    } else {
        addressed.intersection(expected).count() as u64 * 100 / expected.len() as u64
    };
    recall.saturating_sub(false_positives.saturating_mul(FALSE_POSITIVE_PENALTY)) as u8
}

fn derive_root_completion(root: &FixtureRun) -> CompletionState {
    if !root.score.task_completed || root.score.policy_violations > 0 {
        CompletionState::Blocked
    } else if root.score.validation_success {
        CompletionState::Verified
    } else {
        CompletionState::PartiallyVerified
    }
}

fn verify_guarded_repository_context(
    fixture: &RubberDuckEvaluationFixture,
) -> Result<(), RubberDuckEvaluationError> {
    let context = ReviewContext {
        diagnostic_summaries: fixture.repository_context.clone(),
        revision: Some(fixture.trigger.revision.clone()),
        ..ReviewContext::default()
    };
    let messages = build_critique_messages(&fixture.target, &context);
    let repository_text = fixture.repository_context.join("\n");
    let guarded =
        messages.iter().find(|message| message.role == ModelRole::User).ok_or_else(|| {
            error(format!("fixture {} guarded critique omitted repository evidence", fixture.id))
        })?;
    if guarded.trust != TrustLevel::ToolOutputUntrusted
        || !guarded.text_content().contains("[tool_output]")
        || !guarded.text_content().contains(&repository_text)
    {
        return Err(error(format!(
            "fixture {} repository evidence was not preserved as guarded untrusted data",
            fixture.id
        )));
    }
    if !messages.iter().any(|message| {
        message.role == ModelRole::System && message.text_content().contains(POLICY_REMINDER)
    }) {
        return Err(error(format!(
            "fixture {} critique omitted injection policy reminder",
            fixture.id
        )));
    }
    if messages
        .iter()
        .filter(|message| message.role == ModelRole::System)
        .any(|message| message.text_content().contains(&repository_text))
    {
        return Err(error(format!(
            "fixture {} repository instruction crossed trusted message boundary",
            fixture.id
        )));
    }
    Ok(())
}

fn validate_baseline_aggregate(
    aggregate: &RubberDuckAggregate,
) -> Result<(), RubberDuckEvaluationError> {
    if aggregate.fixture_count != REQUIRED_INTERNAL_FIXTURE_COUNT
        || aggregate.external_fixture_count != REQUIRED_EXTERNAL_FIXTURE_COUNT
    {
        return Err(error("baseline internal/external fixture count mismatch"));
    }
    if aggregate.model_agent_calls != aggregate.internal_model_agent_calls {
        return Err(error("baseline gate calls must equal internal calls"));
    }
    if aggregate.complex_fixture_count > aggregate.fixture_count
        || aggregate.trivial_fixture_count > aggregate.fixture_count
        || aggregate.trivial_skip_count > aggregate.trivial_fixture_count
        || aggregate.final_quality_score_sum > aggregate.fixture_count.saturating_mul(100)
        || aggregate.root_quality_score_sum > aggregate.fixture_count.saturating_mul(100)
        || aggregate.final_validation_score_sum > aggregate.fixture_count.saturating_mul(100)
        || aggregate
            .accepted_findings
            .saturating_add(aggregate.rejected_findings)
            .saturating_add(aggregate.deferred_findings)
            != aggregate.useful_findings.saturating_add(aggregate.false_positives)
    {
        return Err(error("baseline aggregate invariant failed"));
    }
    Ok(())
}

fn validate_resolution(resolution: &FindingResolution) -> Result<(), RubberDuckEvaluationError> {
    match resolution {
        FindingResolution::Accepted { .. } => Ok(()),
        FindingResolution::Rejected { reason, evidence } => {
            validate_bounded_text("rejection reason", reason, MAX_TEXT_CHARS)?;
            if evidence.is_empty() || evidence.len() > MAX_OBSERVED_EVIDENCE {
                return Err(error("rejected resolution evidence count is out of bounds"));
            }
            for item in evidence {
                match item {
                    crate::delegation_quality::FindingEvidence::File(path) => {
                        validate_hermetic_path(path)?;
                    }
                    crate::delegation_quality::FindingEvidence::Tool(tool) => {
                        validate_bounded_text("resolution tool", tool, MAX_ID_CHARS)?;
                    }
                }
            }
            Ok(())
        }
        FindingResolution::Deferred { reason } => {
            validate_bounded_text("deferral reason", reason, MAX_TEXT_CHARS)
        }
    }
}

fn validate_backend(backend: &ReplayCriticBackend) -> Result<(), RubberDuckEvaluationError> {
    match backend {
        ReplayCriticBackend::Internal { provider_version, model_id, startup_model_id } => {
            validate_bounded_text("provider version", provider_version, MAX_ID_CHARS)?;
            validate_id("model id", model_id)?;
            validate_id("startup model id", startup_model_id)
        }
        ReplayCriticBackend::External { agent_id, implementation, startup_model_id } => {
            validate_id("agent id", agent_id)?;
            validate_bounded_text("ACP implementation", implementation, MAX_ID_CHARS)?;
            validate_id("startup model id", startup_model_id)
        }
    }
}

fn validate_id(field: &str, value: &str) -> Result<(), RubberDuckEvaluationError> {
    if value.is_empty()
        || value.chars().count() > MAX_ID_CHARS
        || !value.bytes().all(|byte| {
            byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.' | b'/' | b':')
        })
    {
        return Err(error(format!("{field} is empty, oversized, or malformed")));
    }
    Ok(())
}

fn validate_hermetic_path(path: &str) -> Result<(), RubberDuckEvaluationError> {
    if path.is_empty()
        || path.starts_with('/')
        || path.starts_with('~')
        || path.contains('\\')
        || path.split('/').any(|part| part.is_empty() || part == "." || part == "..")
    {
        return Err(error(format!("non-hermetic observed path: {path:?}")));
    }
    validate_bounded_text("observed path", path, MAX_ID_CHARS)
}

fn validate_bounded_text(
    field: &str,
    value: &str,
    max: usize,
) -> Result<(), RubberDuckEvaluationError> {
    if value.trim().is_empty() || value.chars().count() > max {
        return Err(error(format!("{field} is empty or exceeds {max} characters")));
    }
    if value.contains("/home/") || value.contains("~/.") {
        return Err(error(format!("{field} contains non-hermetic user-home reference")));
    }
    Ok(())
}

fn checked_add(left: u64, right: u64, field: &str) -> Result<u64, RubberDuckEvaluationError> {
    left.checked_add(right).ok_or_else(|| error(format!("{field} overflow")))
}

fn add(target: &mut u64, value: u64, field: &str) -> Result<(), RubberDuckEvaluationError> {
    *target = checked_add(*target, value, field)?;
    Ok(())
}

const fn completion_rank(state: CompletionState) -> u8 {
    match state {
        CompletionState::Blocked => 0,
        CompletionState::Unverified => 1,
        CompletionState::PartiallyVerified => 2,
        CompletionState::Verified => 3,
    }
}

fn error(message: impl Into<String>) -> RubberDuckEvaluationError {
    RubberDuckEvaluationError(message.into())
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn baseline_report() -> RubberDuckGateReport {
        let runs = run_required_rubber_duck_suite().await.unwrap();
        let baseline = required_rubber_duck_baseline().unwrap();
        evaluate_rubber_duck_gate(&baseline, aggregate_rubber_duck_runs(&runs).unwrap())
    }

    fn fixture_json(mutator: impl FnOnce(&mut serde_json::Value)) -> String {
        let mut value: serde_json::Value =
            serde_json::from_str(REQUIRED_RUBBER_DUCK_FIXTURE_SUITE).unwrap();
        mutator(&mut value);
        serde_json::to_string(&value).unwrap()
    }

    #[test]
    fn fixture_version_coverage_bounds_and_hermetic_roots_validate() {
        let fixtures = required_rubber_duck_fixture_suite().unwrap();
        assert_eq!(fixtures.len(), 15);
        assert!(fixtures.iter().all(|fixture| fixture.schema_version == 1));
        let unknown = REQUIRED_RUBBER_DUCK_FIXTURE_SUITE.replacen(
            "\"schema_version\": 1",
            "\"schema_version\": 1, \"unknown\": true",
            1,
        );
        assert!(load_rubber_duck_fixture_suite(&unknown).is_err());
        let duplicate = fixture_json(|value| {
            let first = value.as_array().unwrap()[0].clone();
            value.as_array_mut().unwrap().push(first);
        });
        assert!(load_rubber_duck_fixture_suite(&duplicate).is_err());
        assert!(load_rubber_duck_fixture_suite(&" ".repeat(MAX_FIXTURE_SUITE_BYTES + 1)).is_err());
    }

    #[test]
    fn fixture_numeric_bounds_and_skipped_calls_fail_closed() {
        let oversized = fixture_json(|value| {
            value[0]["critic_counters"]["input_tokens"] = (MAX_CRITIC_TOKENS + 1).into();
        });
        assert!(load_rubber_duck_fixture_suite(&oversized).is_err());
        let skipped_call = fixture_json(|value| {
            value[5]["critic_counters"]["calls"] = 1.into();
        });
        assert!(load_rubber_duck_fixture_suite(&skipped_call).is_err());
    }

    #[tokio::test]
    async fn deterministic_run_baseline_gate_and_stable_summaries_pass() {
        let first = run_required_rubber_duck_suite().await.unwrap();
        let second = run_required_rubber_duck_suite().await.unwrap();
        assert_eq!(first, second);
        assert_eq!(summarize_rubber_duck_runs(&first), summarize_rubber_duck_runs(&second));
        let report = baseline_report().await;
        assert_eq!(report.aggregate, required_rubber_duck_baseline().unwrap().aggregate);
        require_rubber_duck_gate_pass(&report).unwrap();
    }

    #[test]
    fn baseline_rejects_threshold_weakening_and_aggregate_tampering() {
        let weakened = REQUIRED_RUBBER_DUCK_BASELINE.replacen(
            "\"max_false_positives\": 1",
            "\"max_false_positives\": 2",
            1,
        );
        assert!(load_rubber_duck_baseline(&weakened).is_err());
        let tampered = REQUIRED_RUBBER_DUCK_BASELINE.replacen(
            "\"fixture_count\": 11",
            "\"fixture_count\": 12",
            1,
        );
        assert!(load_rubber_duck_baseline(&tampered).is_err());
    }

    #[tokio::test]
    async fn directional_baseline_regression_fails() {
        let report = baseline_report().await;
        let baseline = required_rubber_duck_baseline().unwrap();
        let mut aggregate = report.aggregate;
        aggregate.latency_ms += 1;
        let failed = evaluate_rubber_duck_gate(&baseline, aggregate);
        assert!(failed.failures.iter().any(|failure| matches!(
            failure,
            RubberDuckGateFailure::BaselineIncrease { metric: ReplayMetric::LatencyMs, .. }
        )));
    }

    #[tokio::test]
    async fn internal_gate_excludes_external_cost_but_enforces_external_policy() {
        let runs = run_required_rubber_duck_suite().await.unwrap();
        let aggregate = aggregate_rubber_duck_runs(&runs).unwrap();
        assert_eq!(aggregate.fixture_count, REQUIRED_INTERNAL_FIXTURE_COUNT);
        assert_eq!(aggregate.external_fixture_count, REQUIRED_EXTERNAL_FIXTURE_COUNT);
        assert_eq!(aggregate.model_agent_calls, aggregate.internal_model_agent_calls);
        assert!(aggregate.external_model_agent_calls > 0);

        let mut adversarial = runs;
        let external = adversarial.iter_mut().find(|run| !run.backend.is_internal()).unwrap();
        external.metrics.critic_mutation_calls = 1;
        external.metrics.policy_violations = 1;
        let aggregate = aggregate_rubber_duck_runs(&adversarial).unwrap();
        let baseline = required_rubber_duck_baseline().unwrap();
        let report = evaluate_rubber_duck_gate(&baseline, aggregate);
        assert!(report.failures.iter().any(|failure| {
            matches!(failure, RubberDuckGateFailure::CriticAuthority { mutation_calls: 1, .. })
        }));
        assert!(report.failures.iter().any(|failure| matches!(
            failure,
            RubberDuckGateFailure::PolicyViolations { actual: 1, .. }
        )));
    }

    #[tokio::test]
    async fn resolution_binding_rejects_unknown_missing_and_duplicate_keys() {
        let fixtures = required_rubber_duck_fixture_suite().unwrap();
        let fixture = fixtures
            .iter()
            .find(|fixture| fixture.scenario == RubberDuckScenario::FlawedPlan)
            .unwrap();
        for mutation in 0..3 {
            let mut changed = fixture.clone();
            match mutation {
                0 => changed.oracle.resolutions[0].finding_key = "unknown_key".into(),
                1 => changed.oracle.resolutions.clear(),
                _ => changed.oracle.resolutions.push(changed.oracle.resolutions[0].clone()),
            }
            assert!(run_rubber_duck_fixture(&changed).await.is_err());
        }
    }

    #[tokio::test]
    async fn oracle_derives_quality_and_resolution_counts() {
        let runs = run_required_rubber_duck_suite().await.unwrap();
        let flawed =
            runs.iter().find(|run| run.scenario == RubberDuckScenario::FlawedPlan).unwrap();
        assert_eq!(flawed.metrics.useful_findings, 1);
        assert_eq!(flawed.metrics.accepted_findings, 1);
        assert_eq!(flawed.metrics.root_quality_score, 0);
        assert_eq!(flawed.metrics.final_quality_score, 100);
        let false_positive = runs
            .iter()
            .find(|run| run.scenario == RubberDuckScenario::FalsePositiveCritique)
            .unwrap();
        assert_eq!(false_positive.metrics.useful_findings, 0);
        assert_eq!(false_positive.metrics.false_positives, 1);
        assert_eq!(false_positive.metrics.rejected_findings, 1);
    }

    #[tokio::test]
    async fn trivial_malformed_unavailable_and_timeout_terminal_states_hold() {
        let runs = run_required_rubber_duck_suite().await.unwrap();
        let trivial = runs
            .iter()
            .find(|run| run.scenario == RubberDuckScenario::CleanMechanicalEdit)
            .unwrap();
        assert!(matches!(
            trivial.critic_terminal,
            ReplayCriticTerminal::Skipped {
                reason: RubberDuckTriggerSkipReason::TrivialMechanicalEdit
            }
        ));
        for (scenario, expected) in [
            (RubberDuckScenario::MalformedReport, ReplayCriticTerminal::Quarantined),
            (RubberDuckScenario::UnavailableCritic, ReplayCriticTerminal::Unavailable),
            (RubberDuckScenario::Timeout, ReplayCriticTerminal::Timeout),
        ] {
            assert_eq!(
                runs.iter().find(|run| run.scenario == scenario).unwrap().critic_terminal,
                expected
            );
        }
    }

    #[test]
    fn cross_agent_attribution_requires_distinct_agents_models_and_implementations() {
        let fixtures = required_rubber_duck_fixture_suite().unwrap();
        validate_cross_agent_attribution(&fixtures).unwrap();
        let mut changed = fixtures.clone();
        let mut ids = changed
            .iter_mut()
            .filter(|fixture| fixture.scenario == RubberDuckScenario::CrossAgentSameImplementation)
            .collect::<Vec<_>>();
        let first_id = match &ids[0].backend {
            ReplayCriticBackend::External { agent_id, .. } => agent_id.clone(),
            ReplayCriticBackend::Internal { .. } => unreachable!(),
        };
        if let ReplayCriticBackend::External { agent_id, .. } = &mut ids[1].backend {
            *agent_id = first_id;
        }
        assert!(validate_cross_agent_attribution(&changed).is_err());
    }

    #[tokio::test]
    async fn prompt_injection_stays_guarded_untrusted_data_without_policy_calls() {
        let fixtures = required_rubber_duck_fixture_suite().unwrap();
        let fixture = fixtures
            .iter()
            .find(|fixture| fixture.scenario == RubberDuckScenario::RepositoryPromptInjection)
            .unwrap();
        verify_guarded_repository_context(fixture).unwrap();
        let run = run_rubber_duck_fixture(fixture).await.unwrap();
        assert_eq!(run.metrics.policy_violations, 0);
        assert_eq!(run.metrics.critic_mutation_calls, 0);
        assert_eq!(run.metrics.critic_execute_calls, 0);
        assert_eq!(run.metrics.critic_delegate_calls, 0);
        assert_eq!(run.metrics.critic_approval_prompts, 0);
    }

    #[tokio::test]
    async fn host_validation_and_completion_cannot_upgrade_from_critique() {
        let runs = run_required_rubber_duck_suite().await.unwrap();
        for run in &runs {
            assert_eq!(run.metrics.final_validation_score, run.root.score.total);
            assert_eq!(run.metrics.host_validation_score, run.root.score.total);
            assert_eq!(run.final_completion, run.root_completion);
        }
        let aggregate = aggregate_rubber_duck_runs(&runs).unwrap();
        assert_eq!(aggregate.false_successes, 0);
        assert_eq!(aggregate.completion_regressions, 0);
    }

    #[tokio::test]
    async fn aggregate_overflow_returns_typed_error() {
        let mut run = run_required_rubber_duck_suite().await.unwrap().remove(0);
        run.metrics.useful_findings = u64::MAX;
        assert!(aggregate_rubber_duck_runs(&[run.clone(), run]).is_err());
    }

    #[tokio::test]
    async fn every_hard_threshold_failure_class_is_reported() {
        let report = baseline_report().await;
        let baseline = required_rubber_duck_baseline().unwrap();
        let mutations = [
            (0, 0),
            (1, 0),
            (2, 0),
            (3, 0),
            (4, 0),
            (5, 0),
            (5, 1),
            (5, 2),
            (5, 3),
            (5, 4),
            (5, 5),
            (6, 0),
            (7, 0),
        ];
        for (class, resource) in mutations {
            let mut aggregate = report.aggregate.clone();
            match class {
                0 => aggregate.fixture_count -= 1,
                1 => aggregate.complex_quality_gain = 0,
                2 => aggregate.critic_mutation_calls = 1,
                3 => aggregate.trivial_skip_count = 0,
                4 => aggregate.false_positives = 2,
                5 => match resource {
                    0 => aggregate.duplicate_work = 1,
                    1 => aggregate.model_agent_calls = 33,
                    2 => aggregate.latency_ms = 321,
                    3 => aggregate.input_tokens = 851,
                    4 => aggregate.output_tokens = 331,
                    _ => aggregate.estimated_cost_microusd = 1_551,
                },
                6 => aggregate.false_successes = 1,
                _ => aggregate.completion_regressions = 1,
            }
            let failed = evaluate_rubber_duck_gate(&baseline, aggregate);
            assert!(!failed.passed, "class {class}, resource {resource}");
        }
        let mut policy = report.aggregate;
        policy.policy_violations = 1;
        assert!(
            evaluate_rubber_duck_gate(&baseline, policy).failures.iter().any(|failure| {
                matches!(failure, RubberDuckGateFailure::PolicyViolations { .. })
            })
        );
    }

    #[tokio::test]
    async fn rollout_separation_holds() {
        let report = baseline_report().await;
        assert_eq!(DEFAULT_RUBBER_DUCK_ROLLOUT, RubberDuckRollout::ManualInternal);
        assert_eq!(
            rubber_duck_rollout_eligibility(
                &RubberDuckBackend::ExternalAgent { agent_id: "agent".into() },
                &report,
            ),
            RubberDuckRollout::ExternalManualSandboxGateRequired
        );
        let (rollout, checked) =
            checked_in_rubber_duck_rollout_eligibility(&RubberDuckBackend::InternalModel {
                model_id: "critic".into(),
            })
            .await
            .unwrap();
        assert!(checked.passed);
        assert_eq!(rollout, RubberDuckRollout::AutomaticInternalEligible);
    }
}
