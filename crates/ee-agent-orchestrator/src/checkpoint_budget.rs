//! Checkpoint budget validation.
//!
//! Kept out of `checkpoint.rs` (over the 1,000-LOC threshold) so cap
//! comparison stays in one testable place.

use crate::budget::BudgetSnapshot;
use crate::config::OrchestratorConfig;
use crate::error::OrchestratorError;

/// Fails when a stored budget snapshot does not match the checkpoint config or
/// uses more than a configured finite cap.
pub(crate) fn validate_budget(
    budget: &BudgetSnapshot,
    config: &OrchestratorConfig,
) -> Result<(), OrchestratorError> {
    let caps_match = budget.iterations_max == config.max_loop_iterations
        && budget.model_calls_max == config.max_model_calls
        && budget.tool_calls_max == config.max_tool_calls_per_turn
        && budget.output_bytes_max == config.max_output_bytes
        && budget.subagents_max == config.max_subagents
        && budget.input_tokens_max == config.max_input_tokens
        && budget.output_tokens_max == config.max_output_tokens;
    if !caps_match {
        return Err(OrchestratorError::InvalidState(
            "budget snapshot caps differ from checkpoint config".into(),
        ));
    }

    let used_over_cap = budget.iterations_max.is_some_and(|max| budget.iterations_used > max)
        || budget.model_calls_max.is_some_and(|max| budget.model_calls_used > max)
        || budget.tool_calls_max.is_some_and(|max| budget.tool_calls_used > max)
        || budget.output_bytes_max.is_some_and(|max| budget.output_bytes_used > max)
        || budget.subagents_used > budget.subagents_max
        || budget
            .input_tokens_used
            .is_some_and(|used| used > budget.input_tokens_max.unwrap_or(usize::MAX))
        || budget
            .output_tokens_used
            .is_some_and(|used| used > budget.output_tokens_max.unwrap_or(usize::MAX));
    if used_over_cap {
        return Err(OrchestratorError::InvalidState(
            "budget snapshot usage exceeds its caps".into(),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snapshot(config: &OrchestratorConfig) -> BudgetSnapshot {
        BudgetSnapshot {
            iterations_used: 0,
            iterations_max: config.max_loop_iterations,
            model_calls_used: 0,
            model_calls_max: config.max_model_calls,
            tool_calls_used: 0,
            tool_calls_max: config.max_tool_calls_per_turn,
            subagents_used: 0,
            subagents_max: config.max_subagents,
            output_bytes_used: 0,
            output_bytes_max: config.max_output_bytes,
            input_tokens_used: None,
            input_tokens_max: config.max_input_tokens,
            output_tokens_used: None,
            output_tokens_max: config.max_output_tokens,
        }
    }

    #[test]
    fn unlimited_caps_match_and_allow_any_usage() {
        let config = OrchestratorConfig::default();
        let mut budget = snapshot(&config);
        budget.iterations_used = 10_000;
        budget.model_calls_used = 10_000;
        budget.tool_calls_used = 10_000;
        budget.output_bytes_used = 10_000;
        validate_budget(&budget, &config).expect("unlimited caps accept usage");
    }

    #[test]
    fn finite_cap_usage_over_the_limit_fails() {
        let config =
            OrchestratorConfig { max_loop_iterations: Some(1), ..OrchestratorConfig::default() };
        let mut budget = snapshot(&config);
        budget.iterations_used = 2;
        let error = validate_budget(&budget, &config).expect_err("over cap");
        assert!(error.to_string().contains("exceeds its caps"), "{error}");
    }

    #[test]
    fn cap_mismatch_fails() {
        let config = OrchestratorConfig::default();
        let mut budget = snapshot(&config);
        budget.iterations_max = Some(1);
        let error = validate_budget(&budget, &config).expect_err("mismatch");
        assert!(error.to_string().contains("differ from checkpoint config"), "{error}");
    }
}
