//! Subagent delegation depth context.
//!
//! Mirrors DeepSeek Harness's `tool:<name>:depth` contribution: the line is
//! emitted only when delegation is exhausted, never while it is still possible.

/// Returns the depth line only when `current >= max`.
pub(crate) fn depth_line(current: usize, max: usize) -> Option<String> {
    if current < max {
        return None;
    }
    Some(format!(
        "Delegation depth for `delegate_task`: current {current}, maximum {max}, remaining levels 0."
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn below_cap_emits_nothing() {
        assert!(depth_line(0, 2).is_none());
        assert!(depth_line(1, 2).is_none());
    }

    #[test]
    fn at_cap_emits_exact_line() {
        assert_eq!(
            depth_line(2, 2).as_deref(),
            Some("Delegation depth for `delegate_task`: current 2, maximum 2, remaining levels 0.")
        );
    }

    #[test]
    fn over_cap_still_reports_zero_remaining() {
        assert_eq!(
            depth_line(3, 2).as_deref(),
            Some("Delegation depth for `delegate_task`: current 3, maximum 2, remaining levels 0.")
        );
    }
}
