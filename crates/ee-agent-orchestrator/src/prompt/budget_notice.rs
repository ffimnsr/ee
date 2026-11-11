//! Conditional budget pressure notice for runtime context.
//!
//! With unlimited defaults there is nothing to report. A notice appears only
//! when a deployment opted into a finite cap and the turn is close to it.

use crate::budget::BudgetSnapshot;

/// Returns one pressure line when a finite cap is nearly exhausted.
pub(crate) fn pressure_notice(snapshot: &BudgetSnapshot) -> Option<String> {
    let iterations_remaining =
        snapshot.iterations_max.map(|max| max.saturating_sub(snapshot.iterations_used));
    let model_remaining =
        snapshot.model_calls_max.map(|max| max.saturating_sub(snapshot.model_calls_used));
    let tool_remaining =
        snapshot.tool_calls_max.map(|max| max.saturating_sub(snapshot.tool_calls_used));

    let iteration_pressure = iterations_remaining.is_some_and(|remaining| remaining <= 2);
    let tool_pressure = snapshot
        .tool_calls_max
        .is_some_and(|max| max == 0 || snapshot.tool_calls_used.saturating_mul(100) >= max * 90);
    if !iteration_pressure && !tool_pressure {
        return None;
    }

    let model_text =
        model_remaining.map_or_else(|| "unlimited".to_string(), |remaining| remaining.to_string());
    let tool_text =
        tool_remaining.map_or_else(|| "unlimited".to_string(), |remaining| remaining.to_string());
    Some(format!(
        "Turn budget: {model_text} model call(s) and {tool_text} tool call(s) remain. Prefer finishing the current task over starting new exploration."
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snapshot() -> BudgetSnapshot {
        BudgetSnapshot {
            iterations_used: 0,
            iterations_max: None,
            model_calls_used: 0,
            model_calls_max: None,
            tool_calls_used: 0,
            tool_calls_max: None,
            subagents_used: 0,
            subagents_max: 8,
            output_bytes_used: 0,
            output_bytes_max: None,
            input_tokens_used: None,
            input_tokens_max: None,
            output_tokens_used: None,
            output_tokens_max: None,
        }
    }

    #[test]
    fn unlimited_caps_emit_nothing() {
        assert!(pressure_notice(&snapshot()).is_none());
    }

    #[test]
    fn near_iteration_cap_emits_one_line() {
        let mut budget = snapshot();
        budget.iterations_max = Some(16);
        budget.iterations_used = 14;
        budget.model_calls_max = Some(16);
        budget.model_calls_used = 14;
        let notice = pressure_notice(&budget).expect("pressure");
        assert!(notice.contains("2 model call(s)"));
        assert!(notice.contains("unlimited tool call(s)"));
    }

    #[test]
    fn ninety_percent_tool_budget_emits() {
        let mut budget = snapshot();
        budget.tool_calls_max = Some(10);
        budget.tool_calls_used = 9;
        assert!(pressure_notice(&budget).is_some());
        budget.tool_calls_used = 8;
        assert!(pressure_notice(&budget).is_none());
    }

    #[test]
    fn zero_finite_cap_is_pressure() {
        let mut budget = snapshot();
        budget.tool_calls_max = Some(0);
        assert!(pressure_notice(&budget).is_some());
    }
}
