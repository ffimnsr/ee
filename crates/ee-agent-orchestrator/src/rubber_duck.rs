//! Internal contrasting-model rubber-duck execution and root-owned reconciliation.
//!
//! Critic output is untrusted until schema and citation verification succeeds.
//! Only root-owned APIs may resolve findings or create follow-up task nodes.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::sync::watch;

use crate::budget::BudgetTracker;
use crate::completion::{CompletionReport, CompletionState};
use crate::context_planner::ContextInvalidation;
use crate::critic_observability::{
    CriticBackendIdentity, CriticEvent, CriticEventRecorder, CriticSafeReason, CriticUsage,
    RUBBER_DUCK_PROMPT_VERSION, RUBBER_DUCK_ROUTING_VERSION, finding_counts,
};
use crate::critique::{
    CRITIQUE_REPORT_SCHEMA_VERSION, CritiqueFinding, CritiqueReportVerifier, CritiqueSeverity,
    CritiqueTarget, MAX_CRITIQUE_EVIDENCE_CHARS, MAX_CRITIQUE_FINDINGS, MAX_CRITIQUE_OUTPUT_BYTES,
    MAX_CRITIQUE_QUESTION_CHARS, MAX_CRITIQUE_TEXT_CHARS, VerifiedCritiqueReport,
    build_critique_messages,
};
use crate::delegation_quality::{FindingEvidence, ReportEvidence};
use crate::error::OrchestratorError;
use crate::events::{EventRecorder, OrchestratorEvent};
use crate::model::{ModelMessage, ModelRequest, ModelRole, Transcript};
use crate::model_registry::{
    ContrastUnavailable, ModelCapability, ModelInfo, ModelRegistry, RUBBER_DUCK_ROLE,
};
use crate::policy::{PolicyContext, PolicyEngine, ToolPolicy};
use crate::review_context::{MAX_REVIEW_CONTEXT_BYTES, ReviewContext};
use crate::rubber_duck_config::{RubberDuckBackend, RubberDuckConfig, RubberDuckMode};
use crate::subagent_roles::{RUBBER_DUCK_TIMEOUT, rubber_duck_allows_tool};
use crate::tasks::{TaskGraph, TaskId, TaskStatus, truncate};
use crate::tools::{ToolDefinition, ToolRegistry};
use crate::trust::TrustLevel;

/// Critic cache/policy contract version. Increment when request or policy semantics change.
pub const RUBBER_DUCK_POLICY_VERSION: u32 = 1;
/// Maximum cached verified reports per runtime.
pub const MAX_RUBBER_DUCK_CACHE_ENTRIES: usize = 32;
/// Maximum user goal or active plan/task text accepted by runner.
pub const MAX_RUBBER_DUCK_INPUT_CHARS: usize = 8_192;
/// Maximum root-owned finding resolution records.
pub const MAX_RUBBER_DUCK_FINDINGS: usize = 256;
/// Maximum per-session call counters retained by one runtime.
pub const MAX_RUBBER_DUCK_SESSION_COUNTERS: usize = 256;

/// Complete bounded input for one internal critique.
#[derive(Debug, Clone)]
pub struct RubberDuckRequest {
    pub session_id: String,
    pub parent_task_id: TaskId,
    pub target: CritiqueTarget,
    pub user_goal: String,
    pub active_task_or_plan: String,
    pub active_model_id: String,
    pub user_question: Option<String>,
    pub revision: String,
    pub observed_context: ReviewContext,
    pub observed_evidence: ReportEvidence,
    /// Distinguishes deterministic automatic triggers from explicit manual requests.
    pub automatic: bool,
}

/// Successful verified critique plus root-transcript evidence.
#[derive(Debug, Clone, PartialEq)]
pub struct RubberDuckCompleted {
    pub active_model: ModelInfo,
    pub critic_model: ModelInfo,
    pub report: VerifiedCritiqueReport,
    pub transcript_evidence: ModelMessage,
    pub cached: bool,
}

/// Non-failing reason critique could not run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
#[non_exhaustive]
pub enum RubberDuckUnavailable {
    Disabled,
    AutomaticDisabled,
    ExternalBackendConfigured { agent_id: String },
    CallLimitReached { max_calls: usize },
    CallAccountingCapacityReached { max_sessions: usize },
    Contrast(ContrastUnavailable),
    BudgetDenied { reason: String },
    InvalidRequest { reason: String },
}

/// Typed terminal result of one critic attempt.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub enum RubberDuckOutcome {
    Completed(Box<RubberDuckCompleted>),
    Unavailable(RubberDuckUnavailable),
    Quarantined { reason: String },
    Cancelled,
    Failed { reason: String },
}

/// Root-owned disposition for one verified finding.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case", deny_unknown_fields)]
pub enum FindingResolution {
    Accepted { task_id: Option<TaskId> },
    Rejected { reason: String, evidence: Vec<FindingEvidence> },
    Deferred { reason: String },
}

/// One root reconciliation decision.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FindingDecision {
    pub key: String,
    pub resolution: FindingResolution,
}

/// Inputs for one root-owned reconciliation transaction.
pub struct RootFindingReconciliation<'a> {
    pub parent: &'a TaskId,
    pub session_id: &'a str,
    pub revision: &'a str,
    pub target: &'a CritiqueTarget,
    pub decisions: &'a [FindingDecision],
    pub root_evidence: &'a ReportEvidence,
}

/// Persistable root-owned finding state; contains no raw model transport output.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecordedCritiqueFinding {
    pub session_id: String,
    pub revision: String,
    pub target: CritiqueTarget,
    pub finding: CritiqueFinding,
    pub resolution: Option<FindingResolution>,
}

/// Bounded finding ledger used for root reconciliation and completion constraints.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RubberDuckFindingLedger {
    findings: Vec<RecordedCritiqueFinding>,
}

impl RubberDuckFindingLedger {
    /// Stable snapshot in report/finding insertion order.
    #[must_use]
    pub fn findings(&self) -> &[RecordedCritiqueFinding] {
        &self.findings
    }

    fn record_report(&mut self, session_id: &str, revision: &str, report: &VerifiedCritiqueReport) {
        for finding in &report.report().findings {
            let exists = self.findings.iter().any(|record| {
                record.session_id == session_id
                    && record.revision == revision
                    && record.target == report.report().target
                    && record.finding.key == finding.key
            });
            if !exists && self.findings.len() < MAX_RUBBER_DUCK_FINDINGS {
                self.findings.push(RecordedCritiqueFinding {
                    session_id: session_id.to_string(),
                    revision: revision.to_string(),
                    target: report.report().target.clone(),
                    finding: finding.clone(),
                    resolution: None,
                });
            }
        }
    }

    /// Applies root decisions. Accepted blocking/non-blocking findings become
    /// child tasks exactly once; critic code never calls this method.
    pub fn reconcile(
        &mut self,
        tasks: &mut TaskGraph,
        request: RootFindingReconciliation<'_>,
    ) -> Result<Vec<TaskId>, OrchestratorError> {
        let mut created = Vec::new();
        for decision in request.decisions {
            let record = self
                .findings
                .iter_mut()
                .find(|record| {
                    record.session_id == request.session_id
                        && record.revision == request.revision
                        && &record.target == request.target
                        && record.finding.key == decision.key
                })
                .ok_or_else(|| {
                    OrchestratorError::InvalidState(format!(
                        "unknown current-revision rubber-duck finding: {}",
                        decision.key
                    ))
                })?;
            match &decision.resolution {
                FindingResolution::Accepted { .. } => {
                    if matches!(
                        record.resolution,
                        Some(FindingResolution::Accepted { task_id: Some(_) })
                    ) {
                        continue;
                    }
                    let task_id = if matches!(
                        record.finding.severity,
                        CritiqueSeverity::Blocking | CritiqueSeverity::NonBlocking
                    ) {
                        let child = tasks.create_child(
                            request.parent,
                            &format!("resolve critic finding {}", record.finding.key),
                            &record.finding.recommended_change,
                        )?;
                        created.push(child.id.clone());
                        Some(child.id)
                    } else {
                        None
                    };
                    record.resolution = Some(FindingResolution::Accepted { task_id });
                }
                FindingResolution::Rejected { reason, evidence } => {
                    validate_resolution_text("rejection reason", reason)?;
                    if evidence.is_empty()
                        || evidence.iter().any(|item| !request.root_evidence.contains(item))
                    {
                        return Err(OrchestratorError::InvalidState(format!(
                            "rejected finding {} requires observed cited evidence",
                            record.finding.key
                        )));
                    }
                    record.resolution = Some(decision.resolution.clone());
                }
                FindingResolution::Deferred { reason } => {
                    validate_resolution_text("deferral reason", reason)?;
                    record.resolution = Some(decision.resolution.clone());
                }
            }
        }
        Ok(created)
    }

    /// Downgrades verified completion while current-revision blocking findings
    /// remain unresolved. Critique never upgrades host validation evidence.
    pub fn constrain_completion(
        &self,
        tasks: &TaskGraph,
        revision: &str,
        completion: &mut CompletionReport,
    ) {
        if completion.state != CompletionState::Verified {
            return;
        }
        let unresolved = self.findings.iter().find(|record| {
            record.revision == revision
                && record.finding.severity == CritiqueSeverity::Blocking
                && !finding_is_resolved(record, tasks)
        });
        if let Some(record) = unresolved {
            completion.state = CompletionState::Blocked;
            completion.blocker = Some(format!(
                "unresolved current-revision rubber-duck finding: {}",
                record.finding.key
            ));
            completion.safe_follow_up = Some(
                "root must reject with observed evidence or accept and complete finding task"
                    .into(),
            );
        }
    }
}

fn finding_is_resolved(record: &RecordedCritiqueFinding, tasks: &TaskGraph) -> bool {
    match &record.resolution {
        Some(FindingResolution::Rejected { .. }) => true,
        Some(FindingResolution::Accepted { task_id: Some(task_id) }) => {
            tasks.get(task_id).is_some_and(|task| task.status == TaskStatus::Completed)
        }
        Some(FindingResolution::Accepted { task_id: None }) => true,
        Some(FindingResolution::Deferred { .. }) | None => false,
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct RubberDuckCacheKey {
    session_id: String,
    target: CritiqueTarget,
    active_model_id: String,
    critic_model_id: String,
    revision: String,
    plan_task_fingerprint: String,
    policy_version: u32,
}

#[derive(Debug, Clone)]
struct CachedCritique {
    active_model: ModelInfo,
    critic_model: ModelInfo,
    report: VerifiedCritiqueReport,
    transcript_evidence: ModelMessage,
}

#[derive(Debug, Default)]
struct RubberDuckCache {
    entries: HashMap<RubberDuckCacheKey, CachedCritique>,
}

impl RubberDuckCache {
    fn insert(&mut self, key: RubberDuckCacheKey, value: CachedCritique) {
        self.entries.insert(key, value);
        if self.entries.len() > MAX_RUBBER_DUCK_CACHE_ENTRIES {
            let remove = self.entries.keys().min_by_key(|key| {
                (
                    &key.session_id,
                    &key.revision,
                    &key.active_model_id,
                    &key.critic_model_id,
                    &key.plan_task_fingerprint,
                )
            });
            if let Some(remove) = remove.cloned() {
                self.entries.remove(&remove);
            }
        }
    }

    fn invalidate(&mut self, invalidation: &ContextInvalidation) {
        self.entries.retain(|key, _| key.session_id != invalidation.session_id());
    }
}

/// Internal critic service sharing runtime adapters, tools, tasks, budget, and events.
pub struct RubberDuckRunner {
    models: Arc<ModelRegistry>,
    tools: Arc<Mutex<ToolRegistry>>,
    tasks: Arc<Mutex<TaskGraph>>,
    budget: Arc<Mutex<BudgetTracker>>,
    events: EventRecorder,
    cache: Mutex<RubberDuckCache>,
    findings: Mutex<RubberDuckFindingLedger>,
    config: RubberDuckConfig,
    calls_by_session: Mutex<BTreeMap<String, usize>>,
    critic_events: CriticEventRecorder,
}

impl RubberDuckRunner {
    #[cfg(test)]
    pub(crate) fn new(
        models: Arc<ModelRegistry>,
        tools: Arc<Mutex<ToolRegistry>>,
        tasks: Arc<Mutex<TaskGraph>>,
        budget: Arc<Mutex<BudgetTracker>>,
        events: EventRecorder,
    ) -> Self {
        Self::with_config(models, tools, tasks, budget, events, RubberDuckConfig::default())
            .expect("default rubber-duck config is valid")
    }

    pub(crate) fn with_config(
        models: Arc<ModelRegistry>,
        tools: Arc<Mutex<ToolRegistry>>,
        tasks: Arc<Mutex<TaskGraph>>,
        budget: Arc<Mutex<BudgetTracker>>,
        events: EventRecorder,
        config: RubberDuckConfig,
    ) -> Result<Self, crate::rubber_duck_config::RubberDuckConfigError> {
        config.validate()?;
        Ok(Self {
            models,
            tools,
            tasks,
            budget,
            events,
            cache: Mutex::new(RubberDuckCache::default()),
            findings: Mutex::new(RubberDuckFindingLedger::default()),
            config,
            calls_by_session: Mutex::new(BTreeMap::new()),
            critic_events: CriticEventRecorder::default(),
        })
    }

    /// Runs one contrasting-model critique and appends only canonical verified
    /// JSON to root transcript. Raw output and reasoning never cross this API.
    pub async fn run(
        &self,
        request: RubberDuckRequest,
        root_transcript: &mut Transcript,
        cancel: watch::Receiver<bool>,
    ) -> RubberDuckOutcome {
        if self.config.mode == RubberDuckMode::Off {
            self.emit_critic(CriticEvent::Skipped {
                target: request.target.clone(),
                backend: None,
                reason: CriticSafeReason::Disabled,
                policy_version: RUBBER_DUCK_POLICY_VERSION,
            });
            return RubberDuckOutcome::Unavailable(RubberDuckUnavailable::Disabled);
        }
        if request.automatic && self.config.mode != RubberDuckMode::Automatic {
            self.emit_critic(CriticEvent::Skipped {
                target: request.target.clone(),
                backend: None,
                reason: CriticSafeReason::Disabled,
                policy_version: RUBBER_DUCK_POLICY_VERSION,
            });
            return RubberDuckOutcome::Unavailable(RubberDuckUnavailable::AutomaticDisabled);
        }
        if let Some(RubberDuckBackend::ExternalAgent { agent_id }) = &self.config.backend {
            return RubberDuckOutcome::Unavailable(
                RubberDuckUnavailable::ExternalBackendConfigured { agent_id: agent_id.clone() },
            );
        }
        if let Err(reason) = validate_request(&request, self.config.max_context_bytes) {
            self.emit_critic(CriticEvent::Skipped {
                target: request.target.clone(),
                backend: None,
                reason: CriticSafeReason::InvalidRequest,
                policy_version: RUBBER_DUCK_POLICY_VERSION,
            });
            return RubberDuckOutcome::Unavailable(RubberDuckUnavailable::InvalidRequest {
                reason,
            });
        }
        let required = BTreeSet::from([ModelCapability::ChatCompletion, ModelCapability::Tools]);
        let selected = match &self.config.backend {
            Some(RubberDuckBackend::InternalModel { model_id }) => self
                .models
                .select_configured_contrasting(&request.active_model_id, model_id, &required),
            Some(RubberDuckBackend::ExternalAgent { .. }) => unreachable!("handled above"),
            None => self.models.select_contrasting(&request.active_model_id, &required),
        };
        let contrast = match selected {
            Ok(contrast) => contrast,
            Err(reason) => {
                self.emit_critic(CriticEvent::Skipped {
                    target: request.target.clone(),
                    backend: None,
                    reason: CriticSafeReason::Unavailable {
                        code: contrast_reason_code(&reason).into(),
                    },
                    policy_version: RUBBER_DUCK_POLICY_VERSION,
                });
                return RubberDuckOutcome::Unavailable(RubberDuckUnavailable::Contrast(reason));
            }
        };
        let cache_key = RubberDuckCacheKey {
            session_id: request.session_id.clone(),
            target: request.target.clone(),
            active_model_id: contrast.active.id.clone(),
            critic_model_id: contrast.selected.id.clone(),
            revision: request.revision.clone(),
            plan_task_fingerprint: request_fingerprint(&request),
            policy_version: RUBBER_DUCK_POLICY_VERSION,
        };
        if let Some(cached) =
            self.cache.lock().expect("rubber-duck cache poisoned").entries.get(&cache_key).cloned()
        {
            self.findings.lock().expect("rubber-duck findings poisoned").record_report(
                &request.session_id,
                &request.revision,
                &cached.report,
            );
            root_transcript.messages.push(cached.transcript_evidence.clone());
            return RubberDuckOutcome::Completed(Box::new(RubberDuckCompleted {
                active_model: cached.active_model,
                critic_model: cached.critic_model,
                report: cached.report,
                transcript_evidence: cached.transcript_evidence,
                cached: true,
            }));
        }

        {
            let mut calls = self.calls_by_session.lock().expect("rubber-duck calls poisoned");
            if let Err(reason) =
                reserve_session_call(&mut calls, &request.session_id, self.config.max_calls)
            {
                return RubberDuckOutcome::Unavailable(reason);
            }
        }

        {
            let mut budget = self.budget.lock().expect("budget tracker poisoned");
            if let Err(error) = budget.try_reserve_subagent_and_model_calls(1) {
                return RubberDuckOutcome::Unavailable(RubberDuckUnavailable::BudgetDenied {
                    reason: bounded(&error.to_string(), MAX_CRITIQUE_TEXT_CHARS),
                });
            }
            budget.emit(&self.events);
        }

        let child = {
            let mut tasks = self.tasks.lock().expect("task graph poisoned");
            let child = match tasks.create_child(
                &request.parent_task_id,
                RUBBER_DUCK_ROLE,
                &truncate(&request.active_task_or_plan, MAX_CRITIQUE_TEXT_CHARS),
            ) {
                Ok(child) => child,
                Err(error) => return RubberDuckOutcome::Failed { reason: error.to_string() },
            };
            tasks
                .set_model_id(&child.id, Some(contrast.selected.id.clone()))
                .expect("new critic task exists");
            tasks.transition(&child.id, TaskStatus::Running).expect("pending critic starts");
            child
        };
        self.events.record(OrchestratorEvent::SubagentStarted {
            subagent_id: child.id.to_string(),
            role: RUBBER_DUCK_ROLE.to_string(),
            model_id: Some(contrast.selected.id.clone()),
        });
        let backend = internal_identity(&contrast.selected);
        let started = Instant::now();
        self.emit_critic(CriticEvent::Started {
            target: request.target.clone(),
            backend: backend.clone(),
            policy_version: RUBBER_DUCK_POLICY_VERSION,
        });

        let definitions = self.tools.lock().expect("tool registry poisoned").definitions();
        let visible_tools = critic_tools(&definitions);
        let mut messages = build_critique_messages(&request.target, &request.observed_context);
        messages.extend(guarded_request_messages(&request));
        let budget_snapshot = self.budget.lock().expect("budget tracker poisoned").snapshot();
        let model_request =
            ModelRequest::new(messages, visible_tools, budget_snapshot, child.clone())
                .with_model_id(Some(contrast.selected.id.clone()));
        let completion = contrast.adapter.complete(model_request, cancel.clone());
        let response = tokio::select! {
            _ = cancelled(cancel.clone()) => {
                self.finish_task(&child.id, TaskStatus::Cancelled, None, false);
                self.emit_critic(CriticEvent::Cancelled {
                    target: request.target.clone(),
                    backend: backend.clone(),
                    latency_ms: elapsed_ms(started),
                    policy_version: RUBBER_DUCK_POLICY_VERSION,
                });
                return RubberDuckOutcome::Cancelled;
            }
            result = tokio::time::timeout(self.config.timeout.min(RUBBER_DUCK_TIMEOUT), completion) => result,
        };
        let response = match response {
            Err(_) => {
                self.finish_task(&child.id, TaskStatus::Failed, Some("rubber-duck timeout"), false);
                self.emit_critic(CriticEvent::Failed {
                    target: request.target.clone(),
                    backend: backend.clone(),
                    reason: CriticSafeReason::Timeout,
                    latency_ms: elapsed_ms(started),
                    policy_version: RUBBER_DUCK_POLICY_VERSION,
                });
                return RubberDuckOutcome::Failed { reason: "rubber-duck timeout".into() };
            }
            Ok(Err(crate::model::ModelError::Cancelled)) => {
                self.finish_task(&child.id, TaskStatus::Cancelled, None, false);
                self.emit_critic(CriticEvent::Cancelled {
                    target: request.target.clone(),
                    backend: backend.clone(),
                    latency_ms: elapsed_ms(started),
                    policy_version: RUBBER_DUCK_POLICY_VERSION,
                });
                return RubberDuckOutcome::Cancelled;
            }
            Ok(Err(error)) => {
                let reason = bounded(&error.to_string(), MAX_CRITIQUE_TEXT_CHARS);
                self.finish_task(&child.id, TaskStatus::Failed, Some(&reason), false);
                self.emit_critic(CriticEvent::Failed {
                    target: request.target.clone(),
                    backend: backend.clone(),
                    reason: CriticSafeReason::ProviderFailure,
                    latency_ms: elapsed_ms(started),
                    policy_version: RUBBER_DUCK_POLICY_VERSION,
                });
                return RubberDuckOutcome::Failed { reason };
            }
            Ok(Ok(response)) => response,
        };
        if !response.tool_intents.is_empty() || !response.subagent_intents.is_empty() {
            let reason = "rubber-duck response requested tool execution or delegation".to_string();
            self.finish_task(&child.id, TaskStatus::Failed, Some(&reason), false);
            self.emit_critic(CriticEvent::Quarantined {
                target: request.target.clone(),
                backend: backend.clone(),
                reason: CriticSafeReason::VerificationFailed,
                latency_ms: elapsed_ms(started),
                policy_version: RUBBER_DUCK_POLICY_VERSION,
            });
            return RubberDuckOutcome::Quarantined { reason };
        }
        let output_bytes = response.text.len()
            + response.reasoning.as_ref().map_or(0, |reasoning| reasoning.len());
        let output_limit = self.config.max_output_bytes.min(MAX_CRITIQUE_OUTPUT_BYTES);
        if output_bytes > output_limit {
            let reason =
                format!("rubber-duck output is {output_bytes} bytes; maximum is {output_limit}");
            self.finish_task(&child.id, TaskStatus::Failed, Some(&reason), false);
            self.emit_critic(CriticEvent::Quarantined {
                target: request.target.clone(),
                backend: backend.clone(),
                reason: CriticSafeReason::VerificationFailed,
                latency_ms: elapsed_ms(started),
                policy_version: RUBBER_DUCK_POLICY_VERSION,
            });
            return RubberDuckOutcome::Quarantined { reason };
        }
        if let Err(error) = self.budget.lock().expect("budget tracker poisoned").record_model_usage(
            output_bytes,
            response.usage.input_tokens,
            response.usage.output_tokens,
        ) {
            let reason = bounded(&error.to_string(), MAX_CRITIQUE_TEXT_CHARS);
            self.finish_task(&child.id, TaskStatus::Failed, Some(&reason), false);
            return RubberDuckOutcome::Failed { reason };
        }
        let verified = match CritiqueReportVerifier.parse_and_accept_for_target(
            &response.text,
            &request.target,
            &request.observed_evidence,
        ) {
            Ok(verified) => verified,
            Err(error) => {
                let reason = bounded(
                    &format!("rubber-duck critique rejected: {error}"),
                    MAX_CRITIQUE_TEXT_CHARS,
                );
                self.finish_task(&child.id, TaskStatus::Failed, Some(&reason), false);
                self.emit_critic(CriticEvent::Quarantined {
                    target: request.target.clone(),
                    backend: backend.clone(),
                    reason: CriticSafeReason::VerificationFailed,
                    latency_ms: elapsed_ms(started),
                    policy_version: RUBBER_DUCK_POLICY_VERSION,
                });
                return RubberDuckOutcome::Quarantined { reason };
            }
        };
        let canonical = match verified.to_json() {
            Ok(canonical) => canonical,
            Err(error) => {
                let reason = bounded(
                    &format!("verified critique serialization failed: {error}"),
                    MAX_CRITIQUE_TEXT_CHARS,
                );
                self.finish_task(&child.id, TaskStatus::Failed, Some(&reason), false);
                return RubberDuckOutcome::Failed { reason };
            }
        };
        let transcript_evidence = ModelMessage::text(ModelRole::Subagent, canonical.clone())
            .with_trust(TrustLevel::SubagentSummaryUntrusted)
            .with_metadata([
                ("evidence_kind".into(), "rubber_duck_critique".into()),
                ("schema_version".into(), CRITIQUE_REPORT_SCHEMA_VERSION.to_string()),
                ("revision".into(), request.revision.clone()),
                ("active_model_id".into(), contrast.active.id.clone()),
                ("critic_model_id".into(), contrast.selected.id.clone()),
            ]);
        self.findings.lock().expect("rubber-duck findings poisoned").record_report(
            &request.session_id,
            &request.revision,
            &verified,
        );
        self.finish_task(&child.id, TaskStatus::Completed, Some(&canonical), true);
        self.emit_critic(CriticEvent::Completed {
            target: request.target.clone(),
            backend,
            findings: finding_counts(&verified),
            latency_ms: elapsed_ms(started),
            usage: CriticUsage {
                input_tokens: response.usage.input_tokens,
                output_tokens: response.usage.output_tokens,
                estimated_cost_micros: None,
            },
            policy_version: RUBBER_DUCK_POLICY_VERSION,
        });
        root_transcript.messages.push(transcript_evidence.clone());
        self.cache.lock().expect("rubber-duck cache poisoned").insert(
            cache_key,
            CachedCritique {
                active_model: contrast.active.clone(),
                critic_model: contrast.selected.clone(),
                report: verified.clone(),
                transcript_evidence: transcript_evidence.clone(),
            },
        );
        RubberDuckOutcome::Completed(Box::new(RubberDuckCompleted {
            active_model: contrast.active,
            critic_model: contrast.selected,
            report: verified,
            transcript_evidence,
            cached: false,
        }))
    }

    /// Root-owned finding snapshot.
    #[must_use]
    pub fn findings(&self) -> RubberDuckFindingLedger {
        self.findings.lock().expect("rubber-duck findings poisoned").clone()
    }

    /// Root-owned reconciliation entrypoint.
    pub fn reconcile(
        &self,
        parent: &TaskId,
        session_id: &str,
        revision: &str,
        target: &CritiqueTarget,
        decisions: &[FindingDecision],
        root_evidence: &ReportEvidence,
    ) -> Result<Vec<TaskId>, OrchestratorError> {
        let mut tasks = self.tasks.lock().expect("task graph poisoned");
        let created = self.findings.lock().expect("rubber-duck findings poisoned").reconcile(
            &mut tasks,
            RootFindingReconciliation {
                parent,
                session_id,
                revision,
                target,
                decisions,
                root_evidence,
            },
        )?;
        drop(tasks);
        for decision in decisions {
            self.emit_critic(CriticEvent::FindingResolution {
                target: target.clone(),
                resolution: (&decision.resolution).into(),
                policy_version: RUBBER_DUCK_POLICY_VERSION,
            });
        }
        Ok(created)
    }

    /// Applies current ledger blocker to completion report.
    pub fn constrain_completion(&self, revision: &str, completion: &mut CompletionReport) {
        let tasks = self.tasks.lock().expect("task graph poisoned");
        self.findings
            .lock()
            .expect("rubber-duck findings poisoned")
            .constrain_completion(&tasks, revision, completion);
    }

    /// Privacy-safe critic events for telemetry and timeline rendering.
    #[must_use]
    pub fn critic_events(&self) -> Vec<CriticEvent> {
        self.critic_events.events()
    }

    /// Invalidates all cached reports for affected session/revision source.
    pub fn invalidate(&self, invalidation: &ContextInvalidation) {
        self.cache.lock().expect("rubber-duck cache poisoned").invalidate(invalidation);
    }

    fn emit_critic(&self, event: CriticEvent) {
        self.critic_events.record(event.clone());
        self.events.record(OrchestratorEvent::Critic(event));
    }

    fn finish_task(
        &self,
        task_id: &TaskId,
        status: TaskStatus,
        summary: Option<&str>,
        success: bool,
    ) {
        let mut tasks = self.tasks.lock().expect("task graph poisoned");
        if let Some(summary) = summary {
            let _ = tasks.set_result_summary(task_id, summary);
        }
        let _ = tasks.transition(task_id, status);
        drop(tasks);
        self.events.record(OrchestratorEvent::SubagentFinished {
            subagent_id: task_id.to_string(),
            success,
        });
    }
}

fn internal_identity(model: &ModelInfo) -> CriticBackendIdentity {
    CriticBackendIdentity::InternalModel {
        provider_id: model.identity.provider_id.clone(),
        model_id: model.identity.model_id.clone(),
        prompt_version: RUBBER_DUCK_PROMPT_VERSION,
        routing_version: RUBBER_DUCK_ROUTING_VERSION,
    }
}

fn contrast_reason_code(reason: &ContrastUnavailable) -> &'static str {
    match reason {
        ContrastUnavailable::UnknownActiveIdentity { .. } => "unknown_active_identity",
        ContrastUnavailable::NoAlternative => "no_alternative",
        ContrastUnavailable::SameFamilyOnly { .. } => "same_family_only",
        ContrastUnavailable::MissingCapability { .. } => "missing_capability",
        ContrastUnavailable::DisabledRoute => "disabled_route",
    }
}

fn elapsed_ms(started: Instant) -> u64 {
    u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX)
}

fn critic_tools(definitions: &[ToolDefinition]) -> Vec<ToolDefinition> {
    let visible = definitions
        .iter()
        .filter(|definition| rubber_duck_allows_tool(definition))
        .cloned()
        .collect::<Vec<_>>();
    let names = visible.iter().map(|definition| definition.name.clone()).collect::<Vec<_>>();
    let policy = PolicyEngine::new(ToolPolicy {
        allow_read: true,
        allow_write: false,
        allow_execute: false,
        allow_delegate: false,
        allow_host_approved_side_effects: false,
        max_delegate_depth: 0,
        max_parallel_delegates: 0,
        allowed_side_effect_subclasses: Default::default(),
        owned_terminal_ids: Default::default(),
        scope: None,
    })
    .with_allowed_tool_names(names);
    visible
        .into_iter()
        .filter(|definition| policy.check(definition, PolicyContext::default()).allow)
        .collect()
}

fn guarded_request_messages(request: &RubberDuckRequest) -> Vec<ModelMessage> {
    let user_goal = ModelMessage::text(
        ModelRole::User,
        format!("User goal:\n{}", bounded(&request.user_goal, MAX_RUBBER_DUCK_INPUT_CHARS)),
    );
    let active = ModelMessage::text(
        ModelRole::User,
        format!(
            "Active root task or plan (untrusted evidence):\n{}",
            bounded(&request.active_task_or_plan, MAX_RUBBER_DUCK_INPUT_CHARS)
        ),
    )
    .with_trust(TrustLevel::ToolOutputUntrusted);
    crate::prompt_injection::prepare_request(&[user_goal, active]).messages
}

fn reserve_session_call(
    calls: &mut BTreeMap<String, usize>,
    session_id: &str,
    max_calls: usize,
) -> Result<(), RubberDuckUnavailable> {
    if !calls.contains_key(session_id) && calls.len() == MAX_RUBBER_DUCK_SESSION_COUNTERS {
        return Err(RubberDuckUnavailable::CallAccountingCapacityReached {
            max_sessions: MAX_RUBBER_DUCK_SESSION_COUNTERS,
        });
    }
    let used = calls.entry(session_id.to_string()).or_default();
    if *used >= max_calls {
        return Err(RubberDuckUnavailable::CallLimitReached { max_calls });
    }
    *used += 1;
    Ok(())
}

fn validate_request(request: &RubberDuckRequest, max_context_bytes: usize) -> Result<(), String> {
    for (label, value, max) in [
        ("session id", request.session_id.as_str(), MAX_CRITIQUE_EVIDENCE_CHARS),
        ("user goal", request.user_goal.as_str(), MAX_RUBBER_DUCK_INPUT_CHARS),
        ("active task or plan", request.active_task_or_plan.as_str(), MAX_RUBBER_DUCK_INPUT_CHARS),
        ("active model id", request.active_model_id.as_str(), MAX_CRITIQUE_EVIDENCE_CHARS),
        ("revision", request.revision.as_str(), MAX_CRITIQUE_EVIDENCE_CHARS),
    ] {
        if value.trim().is_empty() || value.chars().count() > max {
            return Err(format!("{label} must contain 1 through {max} characters"));
        }
    }
    match (&request.target, &request.user_question) {
        (CritiqueTarget::UserQuestion { question }, Some(request_question))
            if question == request_question
                && question.chars().count() <= MAX_CRITIQUE_QUESTION_CHARS => {}
        (CritiqueTarget::UserQuestion { .. }, _) => {
            return Err("user-question target must match optional user question".into());
        }
        (_, Some(_)) => return Err("user question requires user-question critique target".into()),
        (_, None) => {}
    }
    if request.observed_context.revision.as_deref() != Some(request.revision.as_str()) {
        return Err("observed context revision is missing or stale".into());
    }
    if serde_json::to_vec(&request.observed_context)
        .map_or(true, |value| value.len() > max_context_bytes.min(MAX_REVIEW_CONTEXT_BYTES))
    {
        return Err(format!("observed context exceeds {MAX_REVIEW_CONTEXT_BYTES} bytes"));
    }
    let max_evidence = MAX_CRITIQUE_FINDINGS * 8;
    if request.observed_evidence.files.len() + request.observed_evidence.tools.len() > max_evidence
    {
        return Err(format!("observed evidence exceeds {max_evidence} entries"));
    }
    if request
        .observed_evidence
        .files
        .iter()
        .chain(request.observed_evidence.tools.iter())
        .any(|value| value.trim().is_empty() || value.chars().count() > MAX_CRITIQUE_EVIDENCE_CHARS)
    {
        return Err("observed evidence contains empty or oversized identity".into());
    }
    Ok(())
}

fn validate_resolution_text(label: &str, value: &str) -> Result<(), OrchestratorError> {
    if value.trim().is_empty() || value.chars().count() > MAX_CRITIQUE_TEXT_CHARS {
        return Err(OrchestratorError::InvalidState(format!(
            "{label} must contain 1 through {MAX_CRITIQUE_TEXT_CHARS} characters"
        )));
    }
    Ok(())
}

fn request_fingerprint(request: &RubberDuckRequest) -> String {
    let payload = serde_json::to_vec(&(
        &request.user_goal,
        &request.active_task_or_plan,
        &request.user_question,
        &request.observed_context.task_state,
    ))
    .expect("rubber-duck fingerprint input serializes");
    format!("{:x}", Sha256::digest(payload))
}

fn bounded(value: &str, max: usize) -> String {
    value.chars().take(max).collect()
}

async fn cancelled(mut cancel: watch::Receiver<bool>) {
    if *cancel.borrow() {
        return;
    }
    let _ = cancel.changed().await;
}

#[cfg(test)]
#[path = "rubber_duck_tests.rs"]
mod tests;
