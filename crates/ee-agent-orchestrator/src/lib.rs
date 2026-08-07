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
//!   limits, scoped roles, and bounded summaries.
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
//! - [`trace`] — JSONL trace export of events with secret redaction.
//! - [`strategy`] — deterministic turn strategies, the selector, and
//!   strategy execution wrappers.
//! - [`final_response`] — typed final responses built from observed state
//!   (changed files, recorded validation, provenance).
//! - [`validation`] — validation task planning and execution through the
//!   shared tool executor, with timestamped result records.
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
//!   transcript messages and forwarded through [`UpdateSink`] as they arrive.
//! - [`dialects`] — OpenAI/OpenRouter, Anthropic, and local JSON tool-call
//!   dialect normalization into [`ToolIntent`] values, fail-closed on
//!   malformed payloads.
//! - [`metrics`] — counter-only usage metrics (model calls, tool calls by
//!   class, subagent spawns by role, cancellations, denials, budget stops,
//!   bytes/tokens where known); never holds content.
//! - [`decision_log`] — the bounded [`DecisionLog`]: strategy, tool policy,
//!   routing, and delegation decisions with stable reason codes, redacted
//!   details, and no chain-of-thought.
//! - [`replay`] — deterministic replay scripts over fake model/tools (feature
//!   `test-utils`).
//! - [`test_support`] — deterministic fakes (feature `test-utils`).
//!
//! Phase 1 scope: crate skeleton, config, errors, deterministic state
//! containers, and a runtime that runs one bounded turn with a fake model
//! and fake tools over in-memory framework plumbing.

pub mod budget;
pub mod checkpoint;
pub mod checkpoint_store;
pub mod compaction;
pub mod config;
pub mod context_pack;
pub mod decision_log;
pub mod destructive_policy;
pub mod dialects;
pub mod error;
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
#[cfg(feature = "test-utils")]
pub mod replay;
pub mod retries;
pub mod runtime;
pub mod semantic_memory;
pub mod sensitive_data;
pub mod strategy;
pub mod streaming;
pub mod stuck;
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

// ── Primary public types ────────────────────────────────────────────────

pub use budget::{BudgetConfig, BudgetSnapshot, BudgetTracker};
pub use checkpoint::{
    CHECKPOINT_SCHEMA_VERSION, CompletedToolCall, DEFAULT_CHECKPOINT_PROVENANCE, IdGeneratorState,
    InFlightOperation, OrchestratorCheckpoint, RestoreReport, ResumeState, SubagentTreeState,
    TranscriptSummary, current_unix_millis,
};
pub use checkpoint_store::{CheckpointMeta, CheckpointStore};
pub use compaction::{
    CompactTurnReport, CompactionConfig, DEFAULT_COMPACT_MAX_INPUT_BYTES, SESSION_SUMMARY_KEY,
    build_compaction_context, build_compaction_prompt,
};
pub use config::{OrchestratorConfig, RecoveryConfig};
pub use context_pack::{
    ActiveTaskSummary, ContextItemProvenance, ContextMemoryItem, ContextPack, ContextPackBuilder,
    ContextPackConfig, ContextTruncation, DEFAULT_CONTEXT_PACK_MAX_BYTES,
    DEFAULT_MAX_FILE_REFERENCES, DEFAULT_MAX_MEMORY_ITEMS, DEFAULT_MAX_TOOL_SUMMARIES,
    FILE_REFERENCE_SUMMARY_MAX_CHARS, FileReference, ProvenanceSourceKind, TOOL_SUMMARY_MAX_CHARS,
    ToolSummaryEntry,
};
pub use decision_log::{
    DECISION_DETAIL_MAX_CHARS, DEFAULT_MAX_DECISION_LOG_ENTRIES, DecisionEntry, DecisionKind,
    DecisionLog,
};
pub use destructive_policy::SideEffectSubclass;
pub use dialects::{ToolCallDialect, normalize_tool_calls};
pub use error::OrchestratorError;
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
pub use model_registry::{DEFAULT_MODEL_ID, ModelInfo, ModelRegistry};
pub use model_router::{ModelRoute, ModelRouter, ModelTier, TaskKind, preferred_tier};
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
    ReflectionConfig, ReflectionOutcome, ReviewContext, ReviewFinding, build_review_context,
    build_review_request, create_finding_tasks, findings_from_response, mark_finding_tasks,
};
pub use retries::{
    BackoffStrategy, RetryErrorClass, RetryPolicy, ToolRetrier, classify_tool_error,
};
pub use runtime::OrchestratorRuntime;
pub use semantic_memory::{
    DEFAULT_MAX_SEMANTIC_HITS, DEFAULT_SEMANTIC_LIMIT, SEMANTIC_VALUE_MAX_CHARS, SemanticMemory,
    SemanticMemoryAdapter, SemanticMemoryConfig, SemanticMemoryHit,
};
pub use sensitive_data::{SensitiveDataGuard, is_secret_like, redact_values};
pub use strategy::{
    StrategicInput, StrategyContext, StrategyDecision, StrategyReason, StrategySelector,
    TurnResult, TurnStrategy, has_independent_children, is_validation_tool_name,
    required_capabilities_for,
};
pub use streaming::{
    StreamConsumer, StreamEvent, StreamReceiver, StreamSink, StreamedTurn, StreamingModelAdapter,
    StreamingModelFuture, run_streaming, run_streaming_response, stream_channel,
};
pub use stuck::{StuckConfig, StuckDetector, StuckReason};
pub use subagent_roles::{BuiltinSubagentRole, requires_evidence_for_name};
pub use subagent_verifier::{
    DEFAULT_MAX_CITED_FILES, DEFAULT_MAX_CITED_TOOLS, MAX_CITATION_TOKEN_CHARS, MAX_EVIDENCE_FILES,
    MAX_EVIDENCE_TOOLS, QuarantineEntry, SubagentCitations, SubagentEvidence, SubagentQuarantine,
    SubagentResultVerifier, SubagentVerification,
};
pub use subagents::{
    SubagentId, SubagentIntent, SubagentRequest, SubagentResult, SubagentRole, SubagentStatus,
};
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
    FileTypeRule, ValidationPlan, ValidationPlanEntry, ValidationPlanner, ValidationResult,
    ValidationResultStore, ValidationRunner, default_file_type_rules, finalize_validation_tasks,
};
pub use workspace_scope::WorkspaceScope;
pub use write_conflicts::WriteScopeConflictDetector;
