//! Run summarization and aggregate accounting.
use super::validate::{add, completion_rank, error};
use super::*;
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
