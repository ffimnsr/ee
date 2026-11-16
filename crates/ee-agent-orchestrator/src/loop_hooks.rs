//! Loop lifecycle hooks shared by the engine and its public runtime surface.
//!
//! The engine lives in [`crate::loop_engine`], which is already large; the hook
//! types stay here so the public contract is reviewable on its own and the
//! engine only wires it in.

use std::sync::Arc;

use crate::model::ModelResponse;

/// Boxed loop stop predicate: `true` ends the turn.
pub type UntilFn = Arc<dyn Fn(&[ModelResponse]) -> bool + Send + Sync>;

/// Loop stop predicate, evaluated after each model step.
///
/// Install it with
/// [`OrchestratorRuntime::set_until_hook`](crate::OrchestratorRuntime::set_until_hook).
/// Returning `true` ends the turn with the normal final response, so a caller
/// can stop a multi-step loop deterministically instead of relying on an
/// iteration cap.
#[derive(Clone)]
pub struct UntilPredicate(UntilFn);

impl UntilPredicate {
    /// Wraps one predicate, evaluated with the steps completed so far.
    #[must_use]
    pub fn new(predicate: impl Fn(&[ModelResponse]) -> bool + Send + Sync + 'static) -> Self {
        Self(Arc::new(predicate))
    }

    /// Evaluates the predicate against the steps completed so far.
    #[must_use]
    pub fn is_met(&self, steps: &[ModelResponse]) -> bool {
        (self.0)(steps)
    }
}

impl std::fmt::Debug for UntilPredicate {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("UntilPredicate")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn predicate_sees_the_steps_completed_so_far() {
        let until = UntilPredicate::new(|steps| steps.len() >= 2);
        assert!(!until.is_met(&[]));
        assert!(!until.is_met(&[ModelResponse::new()]));
        assert!(until.is_met(&[ModelResponse::new(), ModelResponse::new()]));
        assert_eq!(format!("{until:?}"), "UntilPredicate");
    }
}
