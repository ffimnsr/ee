//! Deterministic, hermetic evaluation harness for replaying agent task fixtures.
//!
//! Fixtures are versioned JSON data, never live workspaces.  They select one of
//! the existing fake-model/fake-tool replay scripts and carry only redacted,
//! relative workspace snapshots plus deterministic accounting inputs.  This
//! lets CI compare model/provider labels, prompt revisions, routing revisions,
//! and MCP transports without network, home-directory, secret-store, or clock
//! dependencies.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::replay::{
    ReplayOutcome, delegate_then_answer_replay, denied_tool_replay, run_replay,
    simple_answer_replay, tool_then_answer_replay,
};
use crate::sensitive_data::redact;
use crate::trace::export_jsonl;

/// Current fixture and baseline contract version.
pub const EVALUATION_SCHEMA_VERSION: u32 = 1;
/// Checked-in required fixture suite.
pub const REQUIRED_FIXTURE_SUITE: &str = include_str!("../tests/fixtures/replay/v1/tasks.json");
/// Checked-in baseline for the default evaluation profile.
pub const REQUIRED_FIXTURE_BASELINE: &str =
    include_str!("../tests/fixtures/replay/v1/baseline.json");

/// Task category covered by one fixture.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FixtureKind {
    BugFix,
    Feature,
    Refactor,
    CodeReview,
    Investigation,
    MultiFile,
}

/// Deterministic scenario property exercised by one fixture.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ScenarioTag {
    DirtyBuffer,
    StaleRevision,
    WriteConflict,
    InterruptedSession,
    Recovery,
    DeniedApproval,
    UnavailableCapability,
    PromptInjection,
    SecretRedaction,
    PathEscape,
    UnsafeTerminal,
}

/// Fake replay script selected by a fixture.  No variant reaches a provider,
/// editor bridge, terminal, or filesystem.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FixtureScript {
    SimpleAnswer,
    ToolThenAnswer,
    PolicyDenied,
    DelegateThenAnswer,
}

/// Minimal, versioned fixture data.  `workspace` is a redacted source snapshot,
/// not a directory to materialize or execute inside.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EvaluationFixture {
    pub schema_version: u32,
    pub id: String,
    pub kind: FixtureKind,
    pub prompt: String,
    #[serde(default)]
    pub workspace: BTreeMap<String, String>,
    #[serde(default)]
    pub conditions: Vec<ScenarioTag>,
    pub script: FixtureScript,
    pub expected: FixtureExpectation,
    /// Deterministic latency accounting, supplied by fixture data rather than
    /// observed wall time so repeated evidence stays stable.
    pub latency_ms: u64,
    pub input_tokens: u64,
    pub output_tokens: u64,
}

/// Expected stable counters and acceptance facts for one fixture.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FixtureExpectation {
    pub validation_success: bool,
    pub recovery_success: bool,
    pub diff_bytes: u64,
    pub approval_count: u64,
    pub expected_policy_denials: u64,
    pub model_calls: u64,
    pub tool_calls: u64,
}

/// Labels for one candidate under evaluation.  All labels participate in trace
/// fingerprints so evidence cannot be mixed across candidate configurations.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EvaluationProfile {
    pub provider_version: String,
    pub model_version: String,
    pub prompt_version: String,
    pub routing_version: String,
    pub transport: EvaluationTransport,
    /// Micro-USD per input token. Integer accounting avoids float drift.
    pub input_cost_microusd: u64,
    /// Micro-USD per output token. Integer accounting avoids float drift.
    pub output_cost_microusd: u64,
}

/// Transport label for a candidate run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EvaluationTransport {
    Stdio,
    Acp,
}

/// Default candidate whose baseline is checked in CI.  Changing any label
/// changes trace fingerprints and must pass `compare_baseline` first.
pub fn default_evaluation_profile() -> EvaluationProfile {
    EvaluationProfile {
        provider_version: "deterministic-fake-v1".into(),
        model_version: "scripted-model-v1".into(),
        prompt_version: "orchestrator-prompt-v1".into(),
        routing_version: "single-route-v1".into(),
        transport: EvaluationTransport::Acp,
        input_cost_microusd: 1,
        output_cost_microusd: 2,
    }
}

/// Redacted deterministic evidence for one replay. `trace_id` is a stable
/// SHA-256 digest, not a timestamp or mutable path.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReplayTrace {
    pub trace_id: String,
    pub fixture_id: String,
    pub profile: EvaluationProfile,
    pub workspace_snapshot: BTreeMap<String, String>,
    pub event_jsonl: String,
}

/// Measured and deterministic accounting produced by one fixture run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FixtureScore {
    pub task_completed: bool,
    pub validation_success: bool,
    pub policy_violations: u64,
    pub recovery_success: bool,
    pub diff_bytes: u64,
    pub approval_count: u64,
    pub tool_calls: u64,
    pub model_calls: u64,
    pub latency_ms: u64,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub estimated_cost_microusd: u64,
    /// 0..=100 acceptance score; integer avoids platform float differences.
    pub total: u8,
}

/// One fixture result and trace reference.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FixtureRun {
    pub fixture_id: String,
    pub score: FixtureScore,
    pub trace: ReplayTrace,
}

/// A checked-in set of fixture runs used for regression comparison.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EvaluationBaseline {
    pub schema_version: u32,
    pub profile: EvaluationProfile,
    pub runs: Vec<BaselineRun>,
    pub thresholds: RegressionThresholds,
}

/// Stable counters retained in a baseline.  Trace IDs are deliberately absent:
/// candidate labels change them even when behavioral evidence stays equivalent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BaselineRun {
    pub fixture_id: String,
    pub score: FixtureScore,
}

/// Explicit regression limits required before a default changes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RegressionThresholds {
    pub max_score_drop: u8,
    pub max_policy_violation_increase: u64,
    pub max_diff_byte_increase: u64,
    pub max_approval_increase: u64,
    pub max_tool_call_increase: u64,
    pub max_model_call_increase: u64,
    pub max_latency_increase_ms: u64,
    pub max_cost_increase_microusd: u64,
    /// Completion, validation, or recovery may never flip true → false.
    pub require_boolean_non_regression: bool,
}

/// One failed regression threshold with enough data for CI output.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RegressionFailure {
    pub fixture_id: String,
    pub reason: String,
    pub baseline_value: String,
    pub candidate_value: String,
    pub score_delta: i16,
    pub trace_id: String,
}

/// Complete candidate-vs-baseline report.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RegressionReport {
    pub passed: bool,
    pub failures: Vec<RegressionFailure>,
}

/// Parse or contract error. No fixture error is silently accepted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EvaluationError(String);

impl fmt::Display for EvaluationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for EvaluationError {}

/// Parses and validates a versioned fixture suite.
pub fn load_fixture_suite(json: &str) -> Result<Vec<EvaluationFixture>, EvaluationError> {
    let fixtures: Vec<EvaluationFixture> = serde_json::from_str(json)
        .map_err(|error| EvaluationError(format!("invalid evaluation fixture JSON: {error}")))?;
    if fixtures.is_empty() {
        return Err(EvaluationError("evaluation fixture suite must not be empty".into()));
    }
    let mut ids = BTreeSet::new();
    for fixture in &fixtures {
        validate_fixture(fixture)?;
        if !ids.insert(fixture.id.clone()) {
            return Err(EvaluationError(format!(
                "duplicate evaluation fixture id: {}",
                fixture.id
            )));
        }
    }
    Ok(fixtures)
}

/// Loads fixture data committed with the crate.
pub fn required_fixture_suite() -> Result<Vec<EvaluationFixture>, EvaluationError> {
    let fixtures = load_fixture_suite(REQUIRED_FIXTURE_SUITE)?;
    validate_required_coverage(&fixtures)?;
    Ok(fixtures)
}

/// Parses checked-in default baseline.
pub fn required_fixture_baseline() -> Result<EvaluationBaseline, EvaluationError> {
    let baseline: EvaluationBaseline = serde_json::from_str(REQUIRED_FIXTURE_BASELINE)
        .map_err(|error| EvaluationError(format!("invalid evaluation baseline JSON: {error}")))?;
    if baseline.schema_version != EVALUATION_SCHEMA_VERSION {
        return Err(EvaluationError(format!(
            "unsupported evaluation baseline schema version {}",
            baseline.schema_version
        )));
    }
    if baseline.runs.is_empty() {
        return Err(EvaluationError("evaluation baseline must not be empty".into()));
    }
    Ok(baseline)
}

/// Runs one fixture through the existing in-process fake model/tool harness.
pub async fn run_fixture(
    fixture: &EvaluationFixture,
    profile: EvaluationProfile,
) -> Result<FixtureRun, EvaluationError> {
    validate_fixture(fixture)?;
    let outcome = run_script(fixture.script).await;
    let model_calls = outcome.model_requests.len() as u64;
    let tool_calls = outcome
        .events
        .iter()
        .filter(|event| matches!(event, crate::events::OrchestratorEvent::ToolStarted { .. }))
        .count() as u64;
    let policy_denials = policy_denials(&outcome);
    let policy_violations = policy_denials.abs_diff(fixture.expected.expected_policy_denials);
    let task_completed = outcome.prompt_result.is_ok() && outcome.client_requests.is_empty();
    let recovery_required = fixture.conditions.contains(&ScenarioTag::InterruptedSession)
        || fixture.conditions.contains(&ScenarioTag::Recovery);
    let recovery_success = !recovery_required || fixture.expected.recovery_success;
    let estimated_cost_microusd = fixture
        .input_tokens
        .saturating_mul(profile.input_cost_microusd)
        .saturating_add(fixture.output_tokens.saturating_mul(profile.output_cost_microusd));
    let total = acceptance_score(
        task_completed,
        fixture.expected.validation_success,
        policy_violations,
        recovery_success,
    );
    let score = FixtureScore {
        task_completed,
        validation_success: fixture.expected.validation_success,
        policy_violations,
        recovery_success,
        diff_bytes: fixture.expected.diff_bytes,
        approval_count: fixture.expected.approval_count,
        tool_calls,
        model_calls,
        latency_ms: fixture.latency_ms,
        input_tokens: fixture.input_tokens,
        output_tokens: fixture.output_tokens,
        estimated_cost_microusd,
        total,
    };
    if score.model_calls != fixture.expected.model_calls
        || score.tool_calls != fixture.expected.tool_calls
    {
        return Err(EvaluationError(format!(
            "fixture {} script counters changed: expected model/tool {}/{}, got {}/{}",
            fixture.id,
            fixture.expected.model_calls,
            fixture.expected.tool_calls,
            score.model_calls,
            score.tool_calls,
        )));
    }
    let event_jsonl = export_jsonl(&outcome.events).map_err(|error| {
        EvaluationError(format!("fixture {} trace export failed: {error}", fixture.id))
    })?;
    let workspace_snapshot = fixture
        .workspace
        .iter()
        .map(|(path, content)| (path.clone(), redact(content)))
        .collect::<BTreeMap<_, _>>();
    let trace_id = trace_id(&fixture.id, &profile, &workspace_snapshot, &event_jsonl)?;
    Ok(FixtureRun {
        fixture_id: fixture.id.clone(),
        score,
        trace: ReplayTrace {
            trace_id,
            fixture_id: fixture.id.clone(),
            profile,
            workspace_snapshot,
            event_jsonl,
        },
    })
}

/// Runs every fixture in stable id order.
pub async fn run_suite(
    fixtures: &[EvaluationFixture],
    profile: EvaluationProfile,
) -> Result<Vec<FixtureRun>, EvaluationError> {
    let mut fixtures = fixtures.to_vec();
    fixtures.sort_by(|left, right| left.id.cmp(&right.id));
    let mut runs = Vec::with_capacity(fixtures.len());
    for fixture in &fixtures {
        runs.push(run_fixture(fixture, profile.clone()).await?);
    }
    Ok(runs)
}

/// Compares all candidate runs with checked-in baseline thresholds.
pub fn compare_baseline(
    baseline: &EvaluationBaseline,
    candidate: &[FixtureRun],
) -> RegressionReport {
    let candidates =
        candidate.iter().map(|run| (run.fixture_id.as_str(), run)).collect::<BTreeMap<_, _>>();
    let mut failures = Vec::new();
    for base in &baseline.runs {
        let Some(run) = candidates.get(base.fixture_id.as_str()) else {
            failures.push(missing_failure(&base.fixture_id));
            continue;
        };
        compare_run(&baseline.thresholds, base, run, &mut failures);
    }
    RegressionReport { passed: failures.is_empty(), failures }
}

/// Fails closed when regression evidence is missing or violates a threshold.
pub fn require_baseline_pass(report: &RegressionReport) -> Result<(), EvaluationError> {
    if report.passed {
        return Ok(());
    }
    let failures = report
        .failures
        .iter()
        .map(|failure| {
            format!(
                "{}: {} (score delta {}; trace {})",
                failure.fixture_id, failure.reason, failure.score_delta, failure.trace_id
            )
        })
        .collect::<Vec<_>>()
        .join("; ");
    Err(EvaluationError(format!("evaluation regression gate failed: {failures}")))
}

fn validate_fixture(fixture: &EvaluationFixture) -> Result<(), EvaluationError> {
    if fixture.schema_version != EVALUATION_SCHEMA_VERSION {
        return Err(EvaluationError(format!(
            "fixture {} has unsupported schema version {}",
            fixture.id, fixture.schema_version
        )));
    }
    if fixture.id.is_empty()
        || fixture.id.chars().any(|character| !matches!(character, 'a'..='z' | '0'..='9' | '_'))
    {
        return Err(EvaluationError(format!(
            "fixture id must be lowercase snake_case: {}",
            fixture.id
        )));
    }
    if fixture.prompt.is_empty() {
        return Err(EvaluationError(format!("fixture {} has an empty prompt", fixture.id)));
    }
    for path in fixture.workspace.keys() {
        if path.is_empty()
            || path.starts_with('/')
            || path.starts_with('~')
            || path.split('/').any(|part| part.is_empty() || part == "." || part == "..")
        {
            return Err(EvaluationError(format!(
                "fixture {} contains non-hermetic workspace path {path:?}",
                fixture.id
            )));
        }
    }
    if fixture
        .workspace
        .values()
        .any(|content| content.contains("/home/") || content.contains("~/."))
    {
        return Err(EvaluationError(format!("fixture {} depends on a user-home path", fixture.id)));
    }
    Ok(())
}

fn validate_required_coverage(fixtures: &[EvaluationFixture]) -> Result<(), EvaluationError> {
    let kinds = fixtures.iter().map(|fixture| fixture.kind).collect::<BTreeSet<_>>();
    for kind in [
        FixtureKind::BugFix,
        FixtureKind::Feature,
        FixtureKind::Refactor,
        FixtureKind::CodeReview,
        FixtureKind::Investigation,
        FixtureKind::MultiFile,
    ] {
        if !kinds.contains(&kind) {
            return Err(EvaluationError(format!("required fixture kind missing: {kind:?}")));
        }
    }
    let tags = fixtures
        .iter()
        .flat_map(|fixture| fixture.conditions.iter().copied())
        .collect::<BTreeSet<_>>();
    for tag in [
        ScenarioTag::DirtyBuffer,
        ScenarioTag::StaleRevision,
        ScenarioTag::WriteConflict,
        ScenarioTag::InterruptedSession,
        ScenarioTag::Recovery,
        ScenarioTag::DeniedApproval,
        ScenarioTag::UnavailableCapability,
        ScenarioTag::PromptInjection,
        ScenarioTag::SecretRedaction,
        ScenarioTag::PathEscape,
        ScenarioTag::UnsafeTerminal,
    ] {
        if !tags.contains(&tag) {
            return Err(EvaluationError(format!("required scenario tag missing: {tag:?}")));
        }
    }
    Ok(())
}

async fn run_script(script: FixtureScript) -> ReplayOutcome {
    match script {
        FixtureScript::SimpleAnswer => run_replay(simple_answer_replay()).await,
        FixtureScript::ToolThenAnswer => run_replay(tool_then_answer_replay()).await,
        FixtureScript::PolicyDenied => run_replay(denied_tool_replay()).await,
        FixtureScript::DelegateThenAnswer => run_replay(delegate_then_answer_replay()).await,
    }
}

fn policy_denials(outcome: &ReplayOutcome) -> u64 {
    outcome
        .events
        .iter()
        .filter(|event| {
            matches!(event, crate::events::OrchestratorEvent::ToolFinished { success: false, .. })
        })
        .count() as u64
}

fn acceptance_score(completed: bool, validation: bool, violations: u64, recovery: bool) -> u8 {
    let mut total = 0;
    if completed {
        total += 40;
    }
    if validation {
        total += 25;
    }
    if violations == 0 {
        total += 20;
    }
    if recovery {
        total += 15;
    }
    total
}

fn trace_id(
    fixture_id: &str,
    profile: &EvaluationProfile,
    workspace_snapshot: &BTreeMap<String, String>,
    event_jsonl: &str,
) -> Result<String, EvaluationError> {
    let input = serde_json::to_vec(&(fixture_id, profile, workspace_snapshot, event_jsonl))
        .map_err(|error| {
            EvaluationError(format!("trace fingerprint serialization failed: {error}"))
        })?;
    Ok(format!("{:x}", Sha256::digest(input)))
}

fn missing_failure(fixture_id: &str) -> RegressionFailure {
    RegressionFailure {
        fixture_id: fixture_id.into(),
        reason: "required fixture missing from candidate run".into(),
        baseline_value: "present".into(),
        candidate_value: "missing".into(),
        score_delta: -100,
        trace_id: "none".into(),
    }
}

fn compare_run(
    thresholds: &RegressionThresholds,
    baseline: &BaselineRun,
    candidate: &FixtureRun,
    failures: &mut Vec<RegressionFailure>,
) {
    let score_delta = candidate.score.total as i16 - baseline.score.total as i16;
    if score_delta < -(thresholds.max_score_drop as i16) {
        failures.push(failure(
            baseline,
            candidate,
            "total score dropped beyond threshold".into(),
            baseline.score.total.to_string(),
            candidate.score.total.to_string(),
        ));
    }
    check_increase(
        failures,
        baseline,
        candidate,
        "policy violations",
        baseline.score.policy_violations,
        candidate.score.policy_violations,
        thresholds.max_policy_violation_increase,
    );
    check_increase(
        failures,
        baseline,
        candidate,
        "diff bytes",
        baseline.score.diff_bytes,
        candidate.score.diff_bytes,
        thresholds.max_diff_byte_increase,
    );
    check_increase(
        failures,
        baseline,
        candidate,
        "approval count",
        baseline.score.approval_count,
        candidate.score.approval_count,
        thresholds.max_approval_increase,
    );
    check_increase(
        failures,
        baseline,
        candidate,
        "tool calls",
        baseline.score.tool_calls,
        candidate.score.tool_calls,
        thresholds.max_tool_call_increase,
    );
    check_increase(
        failures,
        baseline,
        candidate,
        "model calls",
        baseline.score.model_calls,
        candidate.score.model_calls,
        thresholds.max_model_call_increase,
    );
    check_increase(
        failures,
        baseline,
        candidate,
        "latency",
        baseline.score.latency_ms,
        candidate.score.latency_ms,
        thresholds.max_latency_increase_ms,
    );
    check_increase(
        failures,
        baseline,
        candidate,
        "estimated cost",
        baseline.score.estimated_cost_microusd,
        candidate.score.estimated_cost_microusd,
        thresholds.max_cost_increase_microusd,
    );
    if thresholds.require_boolean_non_regression {
        for (name, base, value) in [
            ("task completion", baseline.score.task_completed, candidate.score.task_completed),
            ("validation", baseline.score.validation_success, candidate.score.validation_success),
            ("recovery", baseline.score.recovery_success, candidate.score.recovery_success),
        ] {
            if base && !value {
                failures.push(failure(
                    baseline,
                    candidate,
                    format!("{name} regressed from passing to failing"),
                    base.to_string(),
                    value.to_string(),
                ));
            }
        }
    }
}

fn check_increase(
    failures: &mut Vec<RegressionFailure>,
    baseline: &BaselineRun,
    candidate: &FixtureRun,
    name: &str,
    base: u64,
    value: u64,
    max_increase: u64,
) {
    if value > base.saturating_add(max_increase) {
        failures.push(failure(
            baseline,
            candidate,
            format!("{name} increased beyond threshold"),
            base.to_string(),
            value.to_string(),
        ));
    }
}

fn failure(
    baseline: &BaselineRun,
    candidate: &FixtureRun,
    reason: String,
    baseline_value: String,
    candidate_value: String,
) -> RegressionFailure {
    RegressionFailure {
        fixture_id: baseline.fixture_id.clone(),
        reason,
        baseline_value,
        candidate_value,
        score_delta: candidate.score.total as i16 - baseline.score.total as i16,
        trace_id: candidate.trace.trace_id.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn required_fixtures_are_versioned_complete_and_hermetic() {
        let fixtures = required_fixture_suite().expect("fixtures load");
        assert!(fixtures.len() >= 6);
        assert!(fixtures.iter().all(|fixture| fixture.schema_version == EVALUATION_SCHEMA_VERSION));
    }

    #[tokio::test]
    async fn repeated_fixture_replay_produces_identical_evidence() {
        let fixture = required_fixture_suite().expect("fixtures load").remove(0);
        let profile = default_evaluation_profile();
        let first = run_fixture(&fixture, profile.clone()).await.expect("first run");
        let second = run_fixture(&fixture, profile).await.expect("second run");
        assert_eq!(first, second, "fixture evidence must be replay-stable");
    }

    #[tokio::test]
    async fn checked_in_baseline_gates_default_profile() {
        let fixtures = required_fixture_suite().expect("fixtures load");
        let baseline = required_fixture_baseline().expect("baseline loads");
        let runs = run_suite(&fixtures, default_evaluation_profile()).await.expect("suite runs");
        let report = compare_baseline(&baseline, &runs);
        require_baseline_pass(&report).expect("default candidate must pass committed baseline");
    }

    #[tokio::test]
    async fn profiles_compare_across_provider_prompt_routing_and_transports() {
        let fixtures = required_fixture_suite().expect("fixtures load");
        let baseline = required_fixture_baseline().expect("baseline loads");
        let acp = run_suite(&fixtures, default_evaluation_profile()).await.expect("acp runs");
        let mut stdio_profile = default_evaluation_profile();
        stdio_profile.provider_version = "deterministic-fake-v2".into();
        stdio_profile.model_version = "scripted-model-v2".into();
        stdio_profile.prompt_version = "orchestrator-prompt-v2".into();
        stdio_profile.routing_version = "route-candidate-v2".into();
        stdio_profile.transport = EvaluationTransport::Stdio;
        let stdio = run_suite(&fixtures, stdio_profile).await.expect("stdio runs");
        assert_ne!(
            acp[0].trace.trace_id, stdio[0].trace.trace_id,
            "candidate labels enter evidence"
        );
        assert!(
            compare_baseline(&baseline, &stdio).passed,
            "same behavior passes across transport"
        );
    }

    #[tokio::test]
    async fn trace_snapshot_redacts_secret_like_fixture_content() {
        let mut workspace = BTreeMap::new();
        workspace.insert("src/config.rs".into(), "OPENROUTER_API_KEY=sk-fixture-123456".into());
        let fixture = EvaluationFixture {
            schema_version: EVALUATION_SCHEMA_VERSION,
            id: "redaction_probe".into(),
            kind: FixtureKind::Investigation,
            prompt: "inspect config".into(),
            workspace,
            conditions: vec![ScenarioTag::SecretRedaction],
            script: FixtureScript::SimpleAnswer,
            expected: FixtureExpectation {
                validation_success: true,
                recovery_success: false,
                diff_bytes: 0,
                approval_count: 0,
                expected_policy_denials: 0,
                model_calls: 1,
                tool_calls: 0,
            },
            latency_ms: 1,
            input_tokens: 1,
            output_tokens: 1,
        };
        let run = run_fixture(&fixture, default_evaluation_profile()).await.expect("fixture runs");
        let redacted = run
            .trace
            .workspace_snapshot
            .get("src/config.rs")
            .expect("snapshot contains fixture file");
        assert!(redacted.contains("[redacted]"));
        assert!(!redacted.contains("sk-fixture-123456"));
    }

    #[test]
    fn regression_report_includes_task_delta_and_trace_reference() {
        let baseline = BaselineRun { fixture_id: "task".into(), score: score(100, 0) };
        let run = FixtureRun {
            fixture_id: "task".into(),
            score: score(60, 1),
            trace: ReplayTrace {
                trace_id: "trace-1".into(),
                fixture_id: "task".into(),
                profile: default_evaluation_profile(),
                workspace_snapshot: BTreeMap::new(),
                event_jsonl: String::new(),
            },
        };
        let report = compare_baseline(
            &EvaluationBaseline {
                schema_version: EVALUATION_SCHEMA_VERSION,
                profile: default_evaluation_profile(),
                runs: vec![baseline],
                thresholds: RegressionThresholds {
                    max_score_drop: 0,
                    max_policy_violation_increase: 0,
                    max_diff_byte_increase: 0,
                    max_approval_increase: 0,
                    max_tool_call_increase: 0,
                    max_model_call_increase: 0,
                    max_latency_increase_ms: 0,
                    max_cost_increase_microusd: 0,
                    require_boolean_non_regression: true,
                },
            },
            &[run],
        );
        assert!(!report.passed);
        assert!(report.failures.iter().any(|failure| failure.trace_id == "trace-1"));
        assert!(report.failures.iter().any(|failure| failure.score_delta < 0));
    }

    fn score(total: u8, policy_violations: u64) -> FixtureScore {
        FixtureScore {
            task_completed: true,
            validation_success: true,
            policy_violations,
            recovery_success: true,
            diff_bytes: 0,
            approval_count: 0,
            tool_calls: 0,
            model_calls: 0,
            latency_ms: 0,
            input_tokens: 0,
            output_tokens: 0,
            estimated_cost_microusd: 0,
            total,
        }
    }
}
