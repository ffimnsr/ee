//! Rubber-duck evaluation tests: contracts, gates, and rollouts.
use super::*;
use crate::rubber_duck_evaluation::validate::{
    validate_cross_agent_attribution, verify_guarded_repository_context,
};

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
    let tampered =
        REQUIRED_RUBBER_DUCK_BASELINE.replacen("\"fixture_count\": 11", "\"fixture_count\": 12", 1);
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
    let fixture =
        fixtures.iter().find(|fixture| fixture.scenario == RubberDuckScenario::FlawedPlan).unwrap();
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
    let flawed = runs.iter().find(|run| run.scenario == RubberDuckScenario::FlawedPlan).unwrap();
    assert_eq!(flawed.metrics.useful_findings, 1);
    assert_eq!(flawed.metrics.accepted_findings, 1);
    assert_eq!(flawed.metrics.root_quality_score, 0);
    assert_eq!(flawed.metrics.final_quality_score, 100);
    let false_positive =
        runs.iter().find(|run| run.scenario == RubberDuckScenario::FalsePositiveCritique).unwrap();
    assert_eq!(false_positive.metrics.useful_findings, 0);
    assert_eq!(false_positive.metrics.false_positives, 1);
    assert_eq!(false_positive.metrics.rejected_findings, 1);
}

#[tokio::test]
async fn trivial_malformed_unavailable_and_timeout_terminal_states_hold() {
    let runs = run_required_rubber_duck_suite().await.unwrap();
    let trivial =
        runs.iter().find(|run| run.scenario == RubberDuckScenario::CleanMechanicalEdit).unwrap();
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
        evaluate_rubber_duck_gate(&baseline, policy)
            .failures
            .iter()
            .any(|failure| { matches!(failure, RubberDuckGateFailure::PolicyViolations { .. }) })
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
