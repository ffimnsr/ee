//! Fixture-suite and baseline loading plus the per-fixture run.
use super::validate::{
    ResolutionKind, bind_resolutions, checked_add, derive_root_completion, error, evaluate_trigger,
    quality_score, resolution_keys, validate_baseline_aggregate, validate_fixture,
    validate_required_coverage, verify_guarded_repository_context, verify_scripted_outcome,
};
use super::*;
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
