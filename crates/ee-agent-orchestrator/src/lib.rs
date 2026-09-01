//! Optional server-side orchestration layer above
//! [`ee-acp-agent-server`](https://docs.rs/ee-acp-agent-server).
//!
//! This crate provides provider-agnostic agent-loop primitives for
//! server-side agent binaries: a bounded model–tool loop, a typed tool
//! registry, task and memory stores, budgets, policy gates, and (in later
//! phases) subagent delegation.  It sits *above* the ACP protocol runtime:
//! [`OrchestratorRuntime::run_turn`] consumes a framework
//! [`PromptContext`](ee_acp_agent_server::PromptContext), streams updates
//! through an [`UpdateSink`](ee_acp_agent_server::UpdateSink), and makes
//! agent → client calls through a
//! [`ClientBridge`](ee_acp_agent_server::ClientBridge).  Model calls are
//! delegated to provider-supplied [`ModelAdapter`] implementations, so the
//! orchestrator never depends on a specific model backend.
//!
//! The crate is optional: providers may serve ACP directly through
//! `ee-acp-agent-server` without it.  It is server-side only and never
//! depends on editor/client-side crates (`ee-agent-host`, `ee-cli`).
//!
//! # Module map
//!
//! - [`mcp`] — Phase 12 MCP bridge: per-session MCP client manager, ACP-native
//!   MCP-over-ACP transport over the framework `ClientBridge`, stdio
//!   transport, provider-compatible tool-name translation, side-effect
//!   classification, and discovery diagnostics.
//! - [`config`] — loop, tool, subagent, timeout, and memory knobs.
//! - [`error`] — [`OrchestratorError`] and its ACP provider mapping.
//! - [`runtime`] — [`OrchestratorRuntime`]: owns the stores and runs turns.
//! - [`loop_engine`] — the bounded model → tool loop.
//! - [`model`] — normalized model messages, the [`Transcript`] builder, and
//!   the [`ModelAdapter`] trait.
//! - [`tools`] — tool definitions, intents, results, and the registry.
//! - [`tasks`] — the deterministic task graph with status transitions.
//! - [`memory`] — the bounded memory store with eviction and compact context.
//! - [`budget`] — per-turn budget tracking: iterations, model calls, tools,
//!   subagents, output bytes, optional token caps, and a wall-clock deadline.
//! - [`policy`] — the conservative default tool policy.
//! - [`events`] — loop event types and the test recorder.
//! - [`subagents`] — logical in-process subagents with depth/parallelism
//!   limits, scoped roles, and bounded structured handoffs.
//! - [`subagent_handoff`] — generic child JSON parsing, backend evidence
//!   attachment, deterministic bounds, and parent payload serialization.
//! - [`subagent_roles`] — the built-in role library: researcher, code_reader,
//!   implementer, test_runner, reviewer, and summarizer tool scopes.
//! - [`subagent_verifier`] — citation verification of child summaries against
//!   their execution evidence, plus the [`SubagentQuarantine`] for failed,
//!   cancelled, and unverified child output.
//! - [`plan_compiler`] — [`PlanCompiler`]: model plans become executable task
//!   graphs; vague tasks, unknown tools, and dependency cycles fail closed.
//! - [`progress_scoring`] — task readiness and blocking over the graph, and
//!   the deterministic [`TaskProgress`] percentage.
//! - [`milestones`] — the [`MilestoneTracker`]: bounded milestone summaries
//!   with provenance and low-value observation compaction under pressure.
//! - [`issue_integration`] — [`IssueChecklist`]: markdown checklist parsing
//!   and criteria-gated completion edits, scoped to configured files.
//! - [`fanout_fanin`] — the [`FanOutFanInCoordinator`]: splits ready
//!   independent tasks into subagent requests, runs them under a parallelism
//!   bound, merges summaries deterministically, and blocks the parent on
//!   required-child failure.
//! - [`write_conflicts`] — the [`WriteScopeConflictDetector`]: overlapping
//!   intended write scopes of concurrent subagents are rejected before spawn
//!   and locks are released on completion or cancellation.
//! - [`provider_adapter`] — [`OrchestratorProvider`]: wraps the runtime as an
//!   ACP [`AgentProvider`](ee_acp_agent_server::AgentProvider).
//! - [`checkpoint`] — serializable session snapshots with fail-closed restore
//!   validation, provenance reports, and deterministic id generator state.
//! - [`completion`] — evidence-gated terminal state (`verified`,
//!   `partially_verified`, `blocked`, or `unverified`) derived only from tool
//!   observations and selected validation records.
//! - [`trace`] — JSONL trace export of events with secret redaction.
//! - [`observability`] — disabled-by-default, in-memory privacy-safe per-turn
//!   waterfall telemetry with version attribution, typed failures, bounded
//!   retention, failed-run replay candidates, redacted evidence references, and
//!   local JSONL export.
//! - [`strategy`] — deterministic turn strategies, the selector, and
//!   strategy execution wrappers.
//! - [`final_response`] — typed final responses built from observed state
//!   (changed files, structured validation, completion evidence, provenance).
//! - [`command_intelligence`] — versioned workspace validation-command metadata,
//!   focused-first escalation, typed command failures, bounded retries, and
//!   redacted/capped execution evidence.
//! - [`validation`] — validation task planning from files, symbols, workspace
//!   declarations, and registered tools; execution routes through the shared
//!   executor and records structured evidence.
//! - [`reflection`] — bounded self-review after tool/edit loops: evidence
//!   assembly, review requests, finding parsing, and task-graph conversion.
//! - [`stuck`] — deterministic stuck detection (repeated responses, repeated
//!   tool calls, repeated failed edits, no-progress iterations).
//! - [`progress`] — progress scoring: confidence from completed tools,
//!   validation outcomes, and review findings, with a `can_finish` gate.
//! - [`trust`] — [`TrustLevel`] labels for transcript and memory content;
//!   tool output and subagent summaries are untrusted by default.
//! - [`prompt_injection`] — the injection guard: labels, delimiters, a
//!   policy reminder, and diagnostic detection for untrusted content.
//! - [`sensitive_data`] — secret-like value detection and redaction before
//!   memory insertion, trace export, and final-response assembly.
//! - [`destructive_policy`] — [`SideEffectSubclass`] gates: delete, move,
//!   overwrite, chmod, terminal kill, and external network are denied by
//!   default.
//! - [`workspace_scope`] — allowed roots and file globs; subagent scopes
//!   narrow from parent scopes and paths outside the scope are rejected
//!   before any client-bridge call.
//! - [`tool_dependencies`] — [`ToolDependency`] metadata on tool definitions
//!   and deterministic execution waves over planned tool batches, with
//!   fail-closed cycle rejection.
//! - [`tool_cache`] — the turn-scoped read-only [`ToolResultCache`] with
//!   path-scope invalidation on writes.
//! - [`parallel_tools`] — [`ParallelToolRunner`]: dependency-aware batches
//!   that run independent read-only tools concurrently and serialize writes.
//! - [`retries`] — classified retries ([`RetryPolicy`], [`ToolRetrier`]) for
//!   transient tool failures; policy denials are never retried.
//! - [`tool_schemas`] — the provider-facing schema compiler with stable
//!   snapshot output.
//! - [`context_pack`] — the [`ContextPackBuilder`]: deterministic,
//!   provenance-rich model context within a byte budget; untrusted content is
//!   labeled and policy reminders precede it.
//! - [`context_planner`] — [`ContextPlanner`]: selects smallest fresh editor,
//!   graph, diff, test, and asset excerpts with explicit trust, revision,
//!   token-cost, selection, and omission metadata; cache reuse requires matching
//!   session, policy, and source revisions.
//! - [`write_transaction`] — [`WriteTransaction`]: revision-bound mutation
//!   evidence with ordered read, preview, approval, apply, diagnostics, diff,
//!   validation, terminal-state, interruption, and safe rollback gates.
//! - [`memory_compaction`] — [`compact_memory`]: duplicate-fact merging and
//!   low-value observation decay under pressure; decisions, constraints, and
//!   validation results are always preserved.
//! - [`semantic_memory`] — the optional [`SemanticMemory`] adapter trait for
//!   external index lookups, merging hits into context packs with provenance.
//! - [`model_router`] — deterministic task-kind/role → adapter routing with
//!   cheap/strong tiers and evented decisions.
//! - [`model_registry`] — named model adapters for subagent selection: the
//!   advertised list reaches the delegating model and unknown selections fail
//!   closed before any child task node exists.
//! - [`rate_limit`] — provider-level concurrency and per-window limits shared
//!   across subagents, with deadline-aware fail-fast queuing.
//! - [`streaming`] — streamed text/reasoning chunks merged into consistent
//!   transcript messages and forwarded through `UpdateSink` as they arrive.
//! - [`dialects`] — OpenAI/OpenRouter, Anthropic, and local JSON tool-call
//!   dialect normalization into [`ToolIntent`] values, fail-closed on
//!   malformed payloads.
//! - [`metrics`] — counter-only usage metrics (model calls, tool calls by
//!   class, subagent spawns by role, cancellations, denials, budget stops,
//!   bytes/tokens where known); never holds content.
//! - [`decision_log`] — the bounded [`DecisionLog`]: strategy, tool policy,
//!   routing, and delegation decisions with stable reason codes, redacted
//!   details, and no chain-of-thought.
//! - [`delegation_quality`] — root-owned delegation preflight estimates,
//!   independent write-owner checks, cited role reports, conflict reconciliation,
//!   and counter-only role effectiveness metrics.
//! - `replay` — deterministic replay scripts over fake model/tools (feature
//!   `test-utils`).
//! - `evaluation` — versioned hermetic task fixtures, redacted evidence,
//!   scorecards, and baseline regression gates (feature `test-utils`).
//! - `test_support` — deterministic fakes (feature `test-utils`).
//!
//! Phase 1 scope: crate skeleton, config, errors, deterministic state
//! containers, and a runtime that runs one bounded turn with a fake model
//! and fake tools over in-memory framework plumbing.

pub mod budget;
pub mod checkpoint;
pub mod checkpoint_store;
mod child_registry;
pub mod command_intelligence;
pub mod compaction;
pub mod completion;
pub mod config;
pub mod context_pack;
pub mod context_planner;
pub mod critic_observability;
pub mod critique;
pub mod decision_log;
pub mod delegation_quality;
pub mod destructive_policy;
pub mod dialects;
pub mod error;
#[cfg(feature = "test-utils")]
pub mod evaluation;
pub mod events;
pub mod fanout_fanin;
pub mod final_response;
pub mod issue_integration;
pub mod loop_engine;
pub mod mcp;
pub mod memory;
pub mod memory_compaction;
pub mod metrics;
pub mod milestones;
pub mod model;
pub mod model_registry;
pub mod model_router;
pub mod observability;
pub mod parallel_tools;
pub mod plan_compiler;
pub mod policy;
pub mod progress;
pub mod progress_scoring;
pub mod prompt_injection;
pub mod provider_adapter;
pub mod rate_limit;
pub mod recovery;
pub mod reflection;
pub mod repair;
pub mod repair_context;
#[cfg(feature = "test-utils")]
pub mod replay;
pub mod retries;
pub mod review_context;
pub mod rubber_duck;
pub mod rubber_duck_config;
#[cfg(feature = "test-utils")]
pub mod rubber_duck_evaluation;
pub mod rubber_duck_trigger;
pub mod runtime;
pub mod semantic_memory;
pub mod sensitive_data;
pub mod session_store;
pub mod strategy;
pub mod streaming;
pub mod stuck;
pub mod subagent_handoff;
pub mod subagent_roles;
pub mod subagent_verifier;
pub mod subagents;
pub mod tasks;
#[cfg(feature = "test-utils")]
pub mod test_support;
pub mod tool_cache;
pub mod tool_dependencies;
pub mod tool_schemas;
pub mod tools;
pub mod trace;
pub mod trust;
pub mod validation;
pub mod workspace_scope;
pub mod write_conflicts;
pub mod write_transaction;

// ── Primary public types ────────────────────────────────────────────────

pub use budget::{BudgetConfig, BudgetSnapshot, BudgetTracker};
pub use checkpoint::{
    CHECKPOINT_SCHEMA_VERSION, CheckpointCaptureMetadata, CheckpointCaptureOrigin,
    CheckpointContextProvenance, CompletedToolCall, DEFAULT_CHECKPOINT_PROVENANCE,
    IdGeneratorState, InFlightOperation, MAX_CHECKPOINT_CONTEXT_SOURCES,
    MAX_CHECKPOINT_EVIDENCE_REFS, MAX_CHECKPOINT_PROVENANCE_LABEL_CHARS, OrchestratorCheckpoint,
    RestoreReport, ResumeState, SubagentTreeState, TranscriptSummary, current_unix_millis,
};
pub use checkpoint_store::{CheckpointMeta, CheckpointStore};
pub use child_registry::{
    ChildCancelResult, ChildProgress, ChildSnapshot, ChildSnapshotEntry, ChildState,
    DEFAULT_CHILD_SNAPSHOT_LIMIT,
};
pub use command_intelligence::{
    VALIDATION_COMMAND_SCHEMA_VERSION, ValidationApprovalClass, ValidationCommandFailure,
    ValidationCommandMetadata, ValidationEscalation, ValidationScope,
};
pub use compaction::{
    CompactTurnReport, CompactionConfig, DEFAULT_COMPACT_MAX_INPUT_BYTES, SESSION_SUMMARY_KEY,
    build_compaction_context, build_compaction_prompt,
};
pub use completion::{
    CompletionEvidence, CompletionEvidenceItem, CompletionReport, CompletionState, EvidenceStatus,
    derive_completion,
};
pub use config::{OrchestratorConfig, RecoveryConfig};
pub use context_pack::{
    ActiveTaskSummary, ContextItemProvenance, ContextMemoryItem, ContextPack, ContextPackBuilder,
    ContextPackConfig, ContextTruncation, DEFAULT_CONTEXT_PACK_MAX_BYTES,
    DEFAULT_MAX_FILE_REFERENCES, DEFAULT_MAX_MEMORY_ITEMS, DEFAULT_MAX_TOOL_SUMMARIES,
    DEFAULT_MAX_WORKSPACE_MEMORY_FACTS, FILE_REFERENCE_SUMMARY_MAX_CHARS, FileReference,
    MAX_WORKSPACE_RECALL_QUERIES, MAX_WORKSPACE_RECALL_QUERIES_PER_SOURCE,
    MAX_WORKSPACE_RECALL_QUERY_CHARS, POTENTIALLY_STALE_WORKSPACE_MEMORY_WARNING,
    ProvenanceSourceKind, TOOL_SUMMARY_MAX_CHARS, ToolSummaryEntry, WorkspaceContextFact,
    WorkspaceFactAuthority, WorkspaceFactFreshness, WorkspaceFactSelectionReason,
    WorkspaceFactState, WorkspaceRecallContext, WorkspaceRecallFreshnessPolicy,
};
pub use context_planner::{
    ContextCandidate, ContextFreshness, ContextInvalidation, ContextPlan, ContextPlanCache,
    ContextPlanIdentity, ContextPlanner, ContextPlannerConfig, ContextPlanningInput,
    ContextSelectionReason, ContextSource, ContextTruncationReason, ContextTrustClass,
    DEFAULT_CONTEXT_PLAN_CACHE_MAX_ENTRIES, DEFAULT_CONTEXT_PLAN_MAX_EXCERPT_CHARS,
    DEFAULT_CONTEXT_PLAN_MAX_ITEMS, DEFAULT_CONTEXT_PLAN_MAX_TOKENS, OmittedContextItem,
    PlannedContextItem,
};
pub use critic_observability::{
    CriticBackendIdentity, CriticEvent, CriticEventRecorder, CriticFindingCounts, CriticSafeReason,
    CriticUsage, RUBBER_DUCK_PROMPT_VERSION, RUBBER_DUCK_ROUTING_VERSION, SafeFindingResolution,
    finding_counts,
};
pub use critique::{
    CRITIQUE_REPORT_SCHEMA_VERSION, CritiqueFinding, CritiqueReport, CritiqueReportError,
    CritiqueReportVerifier, CritiqueSeverity, CritiqueTarget, MAX_CRITIQUE_EVIDENCE_CHARS,
    MAX_CRITIQUE_EVIDENCE_PER_FINDING, MAX_CRITIQUE_FINDINGS, MAX_CRITIQUE_KEY_CHARS,
    MAX_CRITIQUE_OUTPUT_BYTES, MAX_CRITIQUE_QUESTION_CHARS, MAX_CRITIQUE_TEXT_CHARS,
    VerifiedCritiqueReport, build_critique_messages, critique_report_instructions,
};
pub use decision_log::{
    DECISION_DETAIL_MAX_CHARS, DEFAULT_MAX_DECISION_LOG_ENTRIES, DecisionEntry, DecisionKind,
    DecisionLog,
};
pub use delegation_quality::{
    DELEGATION_QUALITY_SCHEMA_VERSION, DelegationBudget, DelegationEffectiveness,
    DelegationEstimate, DelegationPreflight, DelegationPreflightResult, DelegationProposal,
    DelegationQualityImpact, FindingConfidence, FindingEvidence, FindingKind, ReconciledFinding,
    ReconciliationState, RejectedDelegation, ReportEvidence, ReportVerification,
    RoleDelegationEffectiveness, RootResolution, RootSynthesis, SubagentFinding, SubagentReport,
    SubagentReportVerifier, VerifiedSubagentReport, WriteConflictRisk,
};
pub use destructive_policy::SideEffectSubclass;
pub use dialects::{ToolCallDialect, normalize_tool_calls};
pub use error::OrchestratorError;
#[cfg(feature = "test-utils")]
pub use evaluation::{
    EVALUATION_SCHEMA_VERSION, EvaluationBaseline, EvaluationFixture, EvaluationProfile,
    EvaluationTransport, FixtureExpectation, FixtureKind, FixtureRun, FixtureScore, FixtureScript,
    RegressionFailure, RegressionReport, RegressionThresholds, ReplayTrace, ScenarioTag,
    compare_baseline, default_evaluation_profile, load_fixture_suite, require_baseline_pass,
    required_fixture_baseline, required_fixture_suite, run_fixture, run_suite,
};
pub use events::{EventRecorder, OrchestratorEvent};
pub use fanout_fanin::FanOutFanInCoordinator;
pub use final_response::{
    ChangedFile, FinalResponse, FinalResponseBuilder, ValidationOutcome, ValidationRecord,
    ValidationRecorder, changed_files_from_log,
};
pub use issue_integration::{
    CRITERIA_KEY_MARKER, ChecklistEdit, ChecklistItem, IssueChecklist, IssueChecklistConfig,
    is_configured,
};
pub use mcp::{McpToolClassSpec, McpToolPolicy};
pub use memory::{MemoryItem, MemoryStore};
pub use memory_compaction::{
    CompactionReport, MemoryCompactionConfig, PROTECTED_MEMORY_PREFIXES, compact_memory,
    is_protected_key,
};
pub use metrics::OrchestratorMetrics;
pub use milestones::{
    DEFAULT_COMPACTION_PRESSURE, DEFAULT_LOW_VALUE_PREFIX, DEFAULT_MILESTONE_MAX_COMPLETED_TASKS,
    DEFAULT_MILESTONE_MAX_EVENTS, MILESTONE_SUMMARY_MAX_CHARS, MilestoneConfig,
    MilestoneObservation, MilestoneSummary, MilestoneTracker, store_observation,
};
pub use model::{
    ModelAdapter, ModelContent, ModelError, ModelFuture, ModelMessage, ModelRequest, ModelResponse,
    ModelRole, ModelUsage, Transcript, TranscriptTruncation,
};
pub use model_registry::{
    ContrastUnavailable, ContrastingModel, DEFAULT_MODEL_ID, ModelCapability, ModelFamily,
    ModelIdentity, ModelInfo, ModelRegistration, ModelRegistry, RUBBER_DUCK_ROLE,
};
pub use model_router::{ModelRoute, ModelRouter, ModelTier, TaskKind, preferred_tier};
pub use observability::{
    DEFAULT_TELEMETRY_MAX_BYTES_PER_TURN, DEFAULT_TELEMETRY_MAX_EVENTS_PER_TURN,
    DEFAULT_TELEMETRY_MAX_TURNS, OBSERVABILITY_SCHEMA_VERSION, RedactedEvidenceRef,
    ReplayFixtureCandidate, TELEMETRY_LABEL_MAX_CHARS, TelemetryAttribution, TelemetryConfig,
    TelemetryError, TelemetryRecorder, TelemetrySummary, TelemetryTransport, TelemetryTurnOutcome,
    TelemetryVersionLabels, ToolFailureReason, TurnTelemetry, WaterfallEvent, WaterfallFinish,
    WaterfallOutcome, WaterfallStage,
};
pub use parallel_tools::ParallelToolRunner;
pub use plan_compiler::{PlanCompilation, PlanCompiler, PlanInput, TaskCriteria};
pub use policy::{PolicyContext, PolicyDecision, PolicyEngine, ToolPolicy};
pub use progress::{ProgressScore, ProgressTracker};
pub use progress_scoring::{
    TaskProgress, blocked_tasks, is_blocked, is_ready, mark_blocked_by_failed_dependencies,
    ready_tasks,
};
pub use prompt_injection::{
    InjectionDetection, POLICY_REMINDER, UNTRUSTED_LABEL_KEY, detect_injection, prepare_request,
    wrap_untrusted,
};
pub use provider_adapter::{
    DEFAULT_IMPLEMENTATION_NAME, DEFAULT_IMPLEMENTATION_TITLE, OrchestratorProvider,
    OrchestratorProviderConfig, SESSION_ID_PREFIX,
};
pub use rate_limit::{RateLimitClock, RateLimitConfig, RateLimitPermit, RateLimiter, TokioClock};
pub use recovery::{RecoverableInterruption, TurnOutcome, session_timeout_expired};
pub use reflection::{
    ReflectionConfig, ReflectionOutcome, ReviewFinding, build_review_request, create_finding_tasks,
    findings_from_response, mark_finding_tasks,
};
pub use repair::{
    DEFAULT_MAX_REPAIR_ATTEMPTS, MAX_REPAIR_ATTEMPTS, RepairAttempt, RepairConfig,
    RepairController, RepairDecision, RepairFailureSummary, RepairProgress, RepairReason,
    RepairStopReason,
};
pub use repair_context::{
    REPAIR_CONTEXT_TOOLS, RepairContextObservation, RepairContextSnapshot, build_repair_context,
};
pub use retries::{
    BackoffStrategy, RetryErrorClass, RetryPolicy, ToolRetrier, classify_tool_error,
};
pub use review_context::{
    MAX_REVIEW_CONTEXT_BYTES, MAX_REVIEW_CONTEXT_DIAGNOSTICS, MAX_REVIEW_CONTEXT_FILES,
    MAX_REVIEW_CONTEXT_ITEM_CHARS, MAX_REVIEW_CONTEXT_REVISION_CHARS, MAX_REVIEW_CONTEXT_TASKS,
    MAX_REVIEW_CONTEXT_VALIDATIONS, ReviewContext, ReviewContextMetadata, build_review_context,
    build_review_context_with_metadata, render_review_context, review_context_message,
};
pub use rubber_duck::{
    FindingDecision, FindingResolution, MAX_RUBBER_DUCK_CACHE_ENTRIES, MAX_RUBBER_DUCK_FINDINGS,
    MAX_RUBBER_DUCK_INPUT_CHARS, RUBBER_DUCK_POLICY_VERSION, RecordedCritiqueFinding,
    RootFindingReconciliation, RubberDuckCompleted, RubberDuckFindingLedger, RubberDuckOutcome,
    RubberDuckRequest, RubberDuckRunner, RubberDuckUnavailable,
};
pub use rubber_duck_config::{
    DEFAULT_RUBBER_DUCK_CONTEXT_BYTES, DEFAULT_RUBBER_DUCK_MAX_CALLS,
    DEFAULT_RUBBER_DUCK_OUTPUT_BYTES, DEFAULT_RUBBER_DUCK_TIMEOUT, MAX_RUBBER_DUCK_CONTEXT_BYTES,
    MAX_RUBBER_DUCK_MAX_CALLS, MAX_RUBBER_DUCK_OUTPUT_BYTES, MAX_RUBBER_DUCK_TIMEOUT,
    ResolvedRubberDuckConfig, RubberDuckBackend, RubberDuckConfig, RubberDuckConfigError,
    RubberDuckConfigUnavailable, RubberDuckMode,
};
#[cfg(feature = "test-utils")]
pub use rubber_duck_evaluation::{
    DEFAULT_RUBBER_DUCK_ROLLOUT, PINNED_RUBBER_DUCK_GATE_THRESHOLDS,
    REQUIRED_EXTERNAL_FIXTURE_COUNT, REQUIRED_INTERNAL_FIXTURE_COUNT,
    REQUIRED_RUBBER_DUCK_BASELINE, REQUIRED_RUBBER_DUCK_FIXTURE_SUITE,
    RUBBER_DUCK_EVALUATION_SCHEMA_VERSION, ReplayCriticBackend, ReplayCriticTerminal,
    ReplayFindingResolution, ReplayMetric, ReplayObservedEvidence, ReplayOracle, ReplayResource,
    ReplayTriggerExpectation, ReplayTriggerFacts, RubberDuckAggregate,
    RubberDuckEvaluationBaseline, RubberDuckEvaluationError, RubberDuckEvaluationFixture,
    RubberDuckGateFailure, RubberDuckGateReport, RubberDuckGateThresholds, RubberDuckReplayMetrics,
    RubberDuckReplayRun, RubberDuckReplaySummary, RubberDuckRollout, RubberDuckScenario,
    ScriptedCriticCounters, ScriptedCriticOutcome, aggregate_rubber_duck_runs,
    checked_in_rubber_duck_rollout_eligibility, evaluate_rubber_duck_gate,
    load_rubber_duck_baseline, load_rubber_duck_fixture_suite, require_rubber_duck_gate_pass,
    required_rubber_duck_baseline, required_rubber_duck_fixture_suite,
    rubber_duck_rollout_eligibility, run_required_rubber_duck_suite, run_rubber_duck_fixture,
    summarize_rubber_duck_runs,
};
pub use semantic_memory::{
    DEFAULT_WORKSPACE_SEMANTIC_HIT_CAP, MAX_WORKSPACE_SEMANTIC_SIMILARITY,
    WORKSPACE_SEMANTIC_PROJECTION_SCHEMA_VERSION, WorkspaceDigestId, WorkspaceRootSetId,
    WorkspaceSemanticFactProjection, WorkspaceSemanticMemory, WorkspaceSemanticMemoryAdapter,
    WorkspaceSemanticMemoryConfig, WorkspaceSemanticMemoryFilters, WorkspaceSemanticMemoryHit,
    WorkspaceSemanticMemoryQuery, WorkspaceSemanticMemorySearchResult,
    WorkspaceSemanticRebuildReason, WorkspaceSemanticRebuildRequest,
    WorkspaceSemanticSidecarIdentity, WorkspaceSemanticSidecarMetadata,
    WorkspaceSemanticSidecarStaleness,
};
pub use subagent_handoff::{
    GENERIC_HANDOFF_INSTRUCTIONS, HandoffOutputFormat, MAX_HANDOFF_EVIDENCE_ITEMS,
    MAX_HANDOFF_FINDING_CLAIM_CHARS, MAX_HANDOFF_FINDING_KEY_CHARS, MAX_HANDOFF_FINDINGS,
    MAX_HANDOFF_ITEM_CHARS, MAX_HANDOFF_LIST_ITEMS, MAX_HANDOFF_SUMMARY_CHARS,
    MAX_SUBAGENT_HANDOFF_BYTES, SUBAGENT_HANDOFF_SCHEMA_VERSION, SubagentHandoff, SubagentStatus,
};

pub use rubber_duck_trigger::{
    RubberDuckTrigger, RubberDuckTriggerConfig, RubberDuckTriggerController,
    RubberDuckTriggerDecision, RubberDuckTriggerDisposition, RubberDuckTriggerFacts,
    RubberDuckTriggerKey, RubberDuckTriggerMode, RubberDuckTriggerPolicy, RubberDuckTriggerReason,
    RubberDuckTriggerSkipReason, WorkImpact,
};
pub use runtime::{
    AutomaticRubberDuckTurn, ManualRubberDuckTurn, OrchestratorRuntime, StrategicRecoveryContext,
    StrategicRecoveryTurn, StrategicTurnOutcome,
};
pub use semantic_memory::{
    DEFAULT_MAX_SEMANTIC_HITS, DEFAULT_SEMANTIC_LIMIT, SEMANTIC_VALUE_MAX_CHARS, SemanticMemory,
    SemanticMemoryAdapter, SemanticMemoryConfig, SemanticMemoryHit,
};
pub use sensitive_data::{SensitiveDataGuard, is_secret_like, redact_values};
pub use strategy::{
    CapabilityAwareGuidance, StrategicInput, StrategyContext, StrategyDecision, StrategyReason,
    StrategySelector, TurnResult, TurnStrategy, capability_aware_guidance,
    has_independent_children, is_validation_tool_name, required_capabilities_for,
};
pub use streaming::{
    StreamConsumer, StreamEvent, StreamReceiver, StreamSink, StreamedTurn, StreamingModelAdapter,
    StreamingModelFuture, run_streaming, run_streaming_response, stream_channel,
};
pub use stuck::{StuckConfig, StuckDetector, StuckReason};
pub use subagent_roles::{
    BuiltinSubagentRole, RUBBER_DUCK_MAX_CONTEXT_BYTES, RUBBER_DUCK_MAX_ITERATIONS,
    RUBBER_DUCK_MAX_MODEL_CALLS, RUBBER_DUCK_MAX_OUTPUT_BYTES, RUBBER_DUCK_MAX_RECURSION_DEPTH,
    RUBBER_DUCK_MAX_TOOL_CALLS, RUBBER_DUCK_TIMEOUT, RUBBER_DUCK_TOOL_TIMEOUT,
    requires_evidence_for_name, rubber_duck_allows_tool,
};
pub use subagent_verifier::{
    DEFAULT_MAX_CITED_FILES, DEFAULT_MAX_CITED_TOOLS, MAX_CITATION_TOKEN_CHARS, MAX_EVIDENCE_FILES,
    MAX_EVIDENCE_TOOLS, QuarantineEntry, SubagentCitations, SubagentEvidence, SubagentQuarantine,
    SubagentResultVerifier, SubagentVerification,
};
pub use subagents::{SubagentId, SubagentIntent, SubagentRequest, SubagentResult, SubagentRole};
pub use tasks::{TaskGraph, TaskId, TaskNode, TaskStatus, TaskWorker};
pub use tool_cache::{DEFAULT_CACHE_MAX_ENTRIES, ToolCacheKey, ToolResultCache, cache_key};
pub use tool_dependencies::{PlannedTool, ToolDataClass, ToolDependency, ToolDependencyGraph};
pub use tool_schemas::{compile_schemas, compile_tool_schema, validate_compiled_schema};
pub use tools::{
    ServerTool, SideEffectClass, ToolCallContext, ToolDefinition, ToolErrorKind,
    ToolExecutionLogEntry, ToolExecutor, ToolFuture, ToolIntent, ToolRegistry, ToolResult,
};
pub use trace::{TraceLine, export_jsonl, is_sensitive_key, redact_assignments, redact_json};
pub use trust::{TrustLevel, trust_for_role};
pub use validation::{
    DeclaredValidationCommand, DeclaredValidationTask, FileTypeRule,
    VALIDATION_COMMAND_OUTPUT_MAX_BYTES, ValidationPlan, ValidationPlanEntry, ValidationPlanReason,
    ValidationPlanner, ValidationPlanningContext, ValidationResult, ValidationResultStore,
    ValidationRunner, WorkspaceValidationConfig, default_file_type_rules,
    finalize_validation_tasks,
};
pub use workspace_scope::WorkspaceScope;
pub use write_conflicts::WriteScopeConflictDetector;
pub use write_transaction::{
    AppliedWrite, BufferOwnership, RollbackSafetyCheck, SourceRevision, TransactionDiagnostics,
    TransactionFinalDiff, TransactionValidation, WriteApproval, WritePreview, WriteTransaction,
    WriteTransactionError, WriteTransactionState,
};
