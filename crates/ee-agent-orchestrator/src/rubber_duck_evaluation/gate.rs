//! Gate evaluation, pass requirements, and rollout eligibility.
use super::validate::error;
use super::*;
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
