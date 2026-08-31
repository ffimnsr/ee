//! Privacy-safe rubber-duck lifecycle and attribution events.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};

use crate::critique::CritiqueTarget;
use crate::delegation_quality::FindingEvidence;
use crate::rubber_duck::FindingResolution;

/// Prompt contract version recorded for internal critic attribution.
pub const RUBBER_DUCK_PROMPT_VERSION: u32 = 1;
/// Deterministic model-routing contract version.
pub const RUBBER_DUCK_ROUTING_VERSION: u32 = 1;

/// Safe backend identity. Contains no credentials, endpoints, config, or native state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum CriticBackendIdentity {
    InternalModel {
        provider_id: String,
        model_id: String,
        prompt_version: u32,
        routing_version: u32,
    },
    ExternalAgent {
        agent_id: String,
        implementation_name: Option<String>,
        implementation_version: Option<String>,
    },
}

/// Counter-only usage and cost telemetry.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CriticUsage {
    pub input_tokens: Option<usize>,
    pub output_tokens: Option<usize>,
    /// Provider/agent-reported estimated cost in micros of account currency.
    pub estimated_cost_micros: Option<u64>,
}

/// Finding totals safe for timeline and telemetry.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CriticFindingCounts {
    pub blocking: usize,
    pub non_blocking: usize,
    pub suggestions: usize,
}

/// Stable content-free skip/failure reason.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum CriticSafeReason {
    Disabled,
    Unavailable { code: String },
    BudgetExhausted,
    Cancelled,
    Timeout,
    InvalidRequest,
    VerificationFailed,
    ProviderFailure,
    StaleRevision,
}

/// Typed critic lifecycle event. Deliberately cannot carry critique or workspace content.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case", deny_unknown_fields)]
pub enum CriticEvent {
    Started {
        target: CritiqueTarget,
        backend: CriticBackendIdentity,
        policy_version: u32,
    },
    Completed {
        target: CritiqueTarget,
        backend: CriticBackendIdentity,
        findings: CriticFindingCounts,
        latency_ms: u64,
        usage: CriticUsage,
        policy_version: u32,
    },
    Skipped {
        target: CritiqueTarget,
        backend: Option<CriticBackendIdentity>,
        reason: CriticSafeReason,
        policy_version: u32,
    },
    Quarantined {
        target: CritiqueTarget,
        backend: CriticBackendIdentity,
        reason: CriticSafeReason,
        latency_ms: u64,
        policy_version: u32,
    },
    Cancelled {
        target: CritiqueTarget,
        backend: CriticBackendIdentity,
        latency_ms: u64,
        policy_version: u32,
    },
    Failed {
        target: CritiqueTarget,
        backend: CriticBackendIdentity,
        reason: CriticSafeReason,
        latency_ms: u64,
        policy_version: u32,
    },
    FindingResolution {
        target: CritiqueTarget,
        resolution: SafeFindingResolution,
        policy_version: u32,
    },
}

/// Content-free root resolution telemetry. Reasons and cited paths stay out of events.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SafeFindingResolution {
    Accepted,
    Rejected,
    Deferred,
}

impl From<&FindingResolution> for SafeFindingResolution {
    fn from(value: &FindingResolution) -> Self {
        match value {
            FindingResolution::Accepted { .. } => Self::Accepted,
            FindingResolution::Rejected { .. } => Self::Rejected,
            FindingResolution::Deferred { .. } => Self::Deferred,
        }
    }
}

/// Maximum privacy-safe critic lifecycle events retained in memory.
pub const MAX_RETAINED_CRITIC_EVENTS: usize = 256;

/// Shared bounded event recorder used by orchestrator and host broker.
#[derive(Debug, Clone, Default)]
pub struct CriticEventRecorder {
    events: Arc<Mutex<VecDeque<CriticEvent>>>,
}

impl CriticEventRecorder {
    pub fn record(&self, event: CriticEvent) {
        let mut events = self.events.lock().expect("critic event recorder poisoned");
        if events.len() == MAX_RETAINED_CRITIC_EVENTS {
            events.pop_front();
        }
        events.push_back(event);
    }

    #[must_use]
    pub fn events(&self) -> Vec<CriticEvent> {
        self.events.lock().expect("critic event recorder poisoned").iter().cloned().collect()
    }
}

/// Counts findings without retaining their text or citations.
#[must_use]
pub fn finding_counts(report: &crate::critique::VerifiedCritiqueReport) -> CriticFindingCounts {
    let mut counts = CriticFindingCounts::default();
    for finding in &report.report().findings {
        match finding.severity {
            crate::critique::CritiqueSeverity::Blocking => counts.blocking += 1,
            crate::critique::CritiqueSeverity::NonBlocking => counts.non_blocking += 1,
            crate::critique::CritiqueSeverity::Suggestion => counts.suggestions += 1,
        }
    }
    counts
}

// Compile-time reminder: telemetry must never gain evidence-bearing fields.
const _: fn(FindingEvidence) = |_: FindingEvidence| {};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_event_roundtrips_without_content_fields() {
        let backend = CriticBackendIdentity::ExternalAgent {
            agent_id: "critic".into(),
            implementation_name: Some("fake".into()),
            implementation_version: Some("1".into()),
        };
        let events = [
            CriticEvent::Started {
                target: CritiqueTarget::Plan,
                backend: backend.clone(),
                policy_version: 1,
            },
            CriticEvent::Completed {
                target: CritiqueTarget::Implementation,
                backend: backend.clone(),
                findings: CriticFindingCounts { blocking: 1, non_blocking: 2, suggestions: 3 },
                latency_ms: 40,
                usage: CriticUsage {
                    input_tokens: Some(10),
                    output_tokens: Some(5),
                    estimated_cost_micros: Some(7),
                },
                policy_version: 1,
            },
            CriticEvent::Skipped {
                target: CritiqueTarget::Tests,
                backend: None,
                reason: CriticSafeReason::Disabled,
                policy_version: 1,
            },
            CriticEvent::Quarantined {
                target: CritiqueTarget::FailureAnalysis,
                backend: backend.clone(),
                reason: CriticSafeReason::VerificationFailed,
                latency_ms: 2,
                policy_version: 1,
            },
            CriticEvent::Cancelled {
                target: CritiqueTarget::Plan,
                backend: backend.clone(),
                latency_ms: 1,
                policy_version: 1,
            },
            CriticEvent::Failed {
                target: CritiqueTarget::Tests,
                backend: backend.clone(),
                reason: CriticSafeReason::ProviderFailure,
                latency_ms: 3,
                policy_version: 1,
            },
            CriticEvent::FindingResolution {
                target: CritiqueTarget::Implementation,
                resolution: SafeFindingResolution::Rejected,
                policy_version: 1,
            },
        ];
        for event in events {
            let json = serde_json::to_string(&event).unwrap();
            assert!(!json.contains("prompt"));
            assert!(!json.contains("workspace"));
            assert_eq!(serde_json::from_str::<CriticEvent>(&json).unwrap(), event);
        }
    }

    #[test]
    fn recorder_retains_only_newest_bounded_events() {
        let recorder = CriticEventRecorder::default();
        for index in 0..MAX_RETAINED_CRITIC_EVENTS + 7 {
            recorder.record(CriticEvent::Skipped {
                target: CritiqueTarget::Plan,
                backend: None,
                reason: CriticSafeReason::Unavailable { code: index.to_string() },
                policy_version: 1,
            });
        }
        let events = recorder.events();
        assert_eq!(events.len(), MAX_RETAINED_CRITIC_EVENTS);
        assert!(matches!(
            &events[0],
            CriticEvent::Skipped {
                reason: CriticSafeReason::Unavailable { code },
                ..
            } if code == "7"
        ));
    }
}
