//! Privacy-safe, per-turn observability for agent harnesses.
//!
//! This module deliberately does **not** accept prompts, workspace paths, tool
//! arguments, tool output, terminal output, error text, environment values, or
//! session identifiers. Callers record only bounded operation metadata,
//! versioned configuration labels, typed outcomes, and integer counters.
//! Telemetry is disabled by default, retained only in memory, bounded by user
//! configuration, and exported only when a host explicitly requests JSONL.

use std::collections::{BTreeMap, VecDeque};
use std::fmt;

use serde::{Deserialize, Serialize};

use crate::completion::CompletionState;
use crate::sensitive_data::{is_secret_like, redact};
use crate::tools::ToolErrorKind;

/// Stable schema version for [`TurnTelemetry`] JSONL records.
pub const OBSERVABILITY_SCHEMA_VERSION: u32 = 1;
/// Default number of completed turns retained in memory.
pub const DEFAULT_TELEMETRY_MAX_TURNS: usize = 32;
/// Default per-turn waterfall event cap.
pub const DEFAULT_TELEMETRY_MAX_EVENTS_PER_TURN: usize = 128;
/// Default serialized-byte cap per retained turn.
pub const DEFAULT_TELEMETRY_MAX_BYTES_PER_TURN: usize = 64 * 1024;
/// Maximum length accepted for an opaque identifier or version label.
pub const TELEMETRY_LABEL_MAX_CHARS: usize = 128;

/// User-controlled local telemetry retention. Telemetry is memory-only; this
/// configuration never enables network delivery or automatic file writes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct TelemetryConfig {
    /// Whether new turns are recorded. Defaults to `false` for privacy.
    pub enabled: bool,
    /// Maximum completed turns retained. `0` keeps no completed history.
    pub max_turns: usize,
    /// Maximum waterfall events retained for one turn.
    pub max_events_per_turn: usize,
    /// Maximum serialized bytes retained for one turn.
    pub max_bytes_per_turn: usize,
}

impl Default for TelemetryConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            max_turns: DEFAULT_TELEMETRY_MAX_TURNS,
            max_events_per_turn: DEFAULT_TELEMETRY_MAX_EVENTS_PER_TURN,
            max_bytes_per_turn: DEFAULT_TELEMETRY_MAX_BYTES_PER_TURN,
        }
    }
}

impl TelemetryConfig {
    /// Enables or disables memory-only local telemetry while preserving bounded
    /// default retention. This never enables network delivery or persistence.
    #[must_use]
    pub fn with_enabled(mut self, enabled: bool) -> Self {
        self.enabled = enabled;
        self
    }
}

/// Version labels snapshotted when a turn starts. Labels are identifiers, not
/// configuration content: prompts, policies, manifests, provider endpoints,
/// and credentials never enter this structure.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct TelemetryAttribution {
    pub provider_version: String,
    pub model_version: String,
    pub prompt_version: String,
    pub manifest_version: String,
    pub schema_version: String,
    pub policy_version: String,
    pub routing_version: String,
    pub transport: TelemetryTransport,
}

/// Version labels accepted by [`TelemetryAttribution::new`]. Grouping labels
/// keeps call sites explicit without a fragile multi-argument constructor.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct TelemetryVersionLabels {
    pub provider_version: String,
    pub model_version: String,
    pub prompt_version: String,
    pub manifest_version: String,
    pub schema_version: String,
    pub policy_version: String,
    pub routing_version: String,
}

impl TelemetryAttribution {
    /// Creates validated configuration attribution. Every value must be a
    /// short opaque label, not secret-like data or unbounded configuration.
    pub fn new(
        labels: TelemetryVersionLabels,
        transport: TelemetryTransport,
    ) -> Result<Self, TelemetryError> {
        let attribution = Self {
            provider_version: labels.provider_version,
            model_version: labels.model_version,
            prompt_version: labels.prompt_version,
            manifest_version: labels.manifest_version,
            schema_version: labels.schema_version,
            policy_version: labels.policy_version,
            routing_version: labels.routing_version,
            transport,
        };
        attribution.validate()?;
        Ok(attribution)
    }

    /// Validates labels before a turn can retain them.
    pub fn validate(&self) -> Result<(), TelemetryError> {
        for (field, value) in [
            ("provider_version", &self.provider_version),
            ("model_version", &self.model_version),
            ("prompt_version", &self.prompt_version),
            ("manifest_version", &self.manifest_version),
            ("schema_version", &self.schema_version),
            ("policy_version", &self.policy_version),
            ("routing_version", &self.routing_version),
        ] {
            validate_label(field, value)?;
        }
        Ok(())
    }
}

/// MCP transport route attached to one turn's immutable attribution.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TelemetryTransport {
    Stdio,
    Acp,
    Other,
}

/// Stages in a privacy-safe waterfall. Stage values contain no workspace data.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WaterfallStage {
    ModelRouting,
    ModelCall,
    ToolExecution,
    Approval,
    Retry,
    Recovery,
    Validation,
}

/// Completion outcome for a waterfall operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WaterfallOutcome {
    Succeeded,
    Failed,
    Denied,
    Cancelled,
    Skipped,
}

/// Typed tool-failure bucket. These labels are safe for aggregation and stable
/// across transports; callers must not put backend messages in telemetry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolFailureReason {
    InvalidInput,
    PolicyDenial,
    StaleState,
    Timeout,
    TransportFailure,
    UnavailableCapability,
    InternalError,
}

impl ToolFailureReason {
    /// Maps legacy tool errors into a safe telemetry bucket. Sources able to
    /// distinguish stale, transport, or unavailable failures should record
    /// those explicit variants instead of relying on this conservative map.
    #[must_use]
    pub fn from_tool_error_kind(kind: ToolErrorKind) -> Option<Self> {
        match kind {
            ToolErrorKind::InvalidArguments => Some(Self::InvalidInput),
            ToolErrorKind::PermissionDenied => Some(Self::PolicyDenial),
            ToolErrorKind::Timeout => Some(Self::Timeout),
            ToolErrorKind::Backend => Some(Self::InternalError),
            ToolErrorKind::Cancelled => None,
        }
    }
}

/// One redacted waterfall event. `operation_id` is host-local and opaque; it
/// must not be derived from a session id, path, prompt, or tool-call argument.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct WaterfallEvent {
    /// Stable per-turn ordering, assigned by [`TelemetryRecorder`].
    pub sequence: u64,
    /// Monotonic elapsed time supplied by host/test clock. No wall-clock time.
    pub elapsed_ms: u64,
    pub stage: WaterfallStage,
    pub operation_id: u64,
    /// `true` starts an operation; `false` closes it with `outcome`.
    pub started: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub outcome: Option<WaterfallOutcome>,
    /// Present only for failed tool operations; no message is retained.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_failure: Option<ToolFailureReason>,
    /// Sanitized bounded declared tool identifier. Never arguments or output.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_name: Option<String>,
}

/// Counter-only summary supplied when one turn completes.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct TelemetrySummary {
    /// Optional externally scored quality (0..=100); absent means unknown.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub quality_score: Option<u8>,
    pub latency_ms: u64,
    pub approval_count: u64,
    pub retry_count: u64,
    pub repair_count: u64,
    pub recovery_count: u64,
    pub validation_count: u64,
    pub tool_calls: u64,
    pub model_calls: u64,
    pub estimated_cost_microusd: u64,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub tool_failures: BTreeMap<ToolFailureReason, u64>,
}

impl TelemetrySummary {
    /// Adds one typed tool failure without retaining its message.
    pub fn record_tool_failure(&mut self, reason: ToolFailureReason) {
        *self.tool_failures.entry(reason).or_default() += 1;
    }

    /// Validates bounded score input.
    pub fn validate(&self) -> Result<(), TelemetryError> {
        if self.quality_score.is_some_and(|score| score > 100) {
            return Err(TelemetryError::InvalidQualityScore);
        }
        Ok(())
    }
}

/// A checked-in replay fixture that may reproduce a failed live turn. This is
/// a candidate link only, never proof that a live incident was reproduced.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct ReplayFixtureCandidate {
    pub schema_version: u32,
    pub fixture_id: String,
}

impl ReplayFixtureCandidate {
    /// Creates an opaque, lowercase-snake-case fixture reference.
    pub fn new(schema_version: u32, fixture_id: impl Into<String>) -> Result<Self, TelemetryError> {
        let fixture_id = fixture_id.into();
        if schema_version == 0 || !is_fixture_id(&fixture_id) {
            return Err(TelemetryError::InvalidFixtureReference);
        }
        Ok(Self { schema_version, fixture_id })
    }
}

/// Reference to evidence already redacted by a host. It is an opaque id, never
/// a path, URL, raw trace, terminal output, or workspace snapshot.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct RedactedEvidenceRef {
    pub artifact_id: String,
}

impl RedactedEvidenceRef {
    /// Validates a short opaque artifact id. Value content is not accepted.
    pub fn new(artifact_id: impl Into<String>) -> Result<Self, TelemetryError> {
        let artifact_id = artifact_id.into();
        validate_label("artifact_id", &artifact_id)?;
        Ok(Self { artifact_id })
    }
}

/// Terminal outcome of one observed turn.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TelemetryTurnOutcome {
    Succeeded,
    Failed,
    Cancelled,
}

/// Input describing a completed waterfall operation. It has no field for
/// arguments, output, paths, messages, prompts, or environment data.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct WaterfallFinish {
    pub elapsed_ms: u64,
    pub stage: WaterfallStage,
    pub operation_id: u64,
    pub outcome: WaterfallOutcome,
    pub tool_failure: Option<ToolFailureReason>,
    pub tool_name: Option<String>,
}

/// A completed local turn record exported as one stable JSONL line.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct TurnTelemetry {
    pub schema_version: u32,
    /// Host-local opaque id. No session, task, prompt, or path data allowed.
    pub turn_id: String,
    pub attribution: TelemetryAttribution,
    pub outcome: TelemetryTurnOutcome,
    /// Host-derived terminal completion state, when the provider reached one.
    /// This is never inferred from model prose or ACP `stopReason`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub terminal_state: Option<CompletionState>,
    pub waterfall: Vec<WaterfallEvent>,
    pub summary: TelemetrySummary,
    /// True when retention caps dropped waterfall events.
    pub truncated: bool,
    pub dropped_events: u64,
    /// Present only for failed turns.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub replay_fixture_candidate: Option<ReplayFixtureCandidate>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub evidence_artifacts: Vec<RedactedEvidenceRef>,
}

/// Privacy or retention error from the telemetry API.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum TelemetryError {
    InvalidConfig(&'static str),
    InvalidLabel { field: &'static str },
    InvalidQualityScore,
    InvalidFixtureReference,
    UnknownTurn,
    DuplicateTurn,
    FixtureCandidateRequiresFailure,
    EvidenceArtifactsRequireFailure,
    Serialization(String),
}

impl fmt::Display for TelemetryError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidConfig(reason) => {
                write!(formatter, "invalid telemetry configuration: {reason}")
            }
            Self::InvalidLabel { field } => {
                write!(formatter, "invalid sensitive telemetry label: {field}")
            }
            Self::InvalidQualityScore => {
                formatter.write_str("telemetry quality score must be at most 100")
            }
            Self::InvalidFixtureReference => {
                formatter.write_str("invalid replay fixture candidate reference")
            }
            Self::UnknownTurn => formatter.write_str("unknown telemetry turn"),
            Self::DuplicateTurn => formatter.write_str("duplicate telemetry turn id"),
            Self::FixtureCandidateRequiresFailure => {
                formatter.write_str("replay fixture candidate requires a failed turn")
            }
            Self::EvidenceArtifactsRequireFailure => {
                formatter.write_str("evidence artifacts require a failed turn")
            }
            Self::Serialization(error) => {
                write!(formatter, "telemetry serialization failed: {error}")
            }
        }
    }
}

impl std::error::Error for TelemetryError {}

/// Explicit local, in-memory telemetry recorder. Use [`Self::clear`] to remove
/// retained history. This type never persists or transmits telemetry itself.
#[derive(Debug, Clone)]
pub struct TelemetryRecorder {
    config: TelemetryConfig,
    active: BTreeMap<String, ActiveTurn>,
    completed: VecDeque<TurnTelemetry>,
}

#[derive(Debug, Clone)]
struct ActiveTurn {
    record: TurnTelemetry,
    next_sequence: u64,
}

impl Default for TelemetryRecorder {
    fn default() -> Self {
        Self::new(TelemetryConfig::default()).expect("default telemetry configuration is valid")
    }
}

impl TelemetryRecorder {
    /// Creates a recorder after validating user-controlled retention caps.
    pub fn new(config: TelemetryConfig) -> Result<Self, TelemetryError> {
        validate_config(&config)?;
        Ok(Self { config, active: BTreeMap::new(), completed: VecDeque::new() })
    }

    /// Current immutable retention/user-control configuration.
    #[must_use]
    pub fn config(&self) -> &TelemetryConfig {
        &self.config
    }

    /// Starts a local turn. Returns `false` when telemetry is disabled; callers
    /// can still run normally without allocating a telemetry record.
    pub fn start_turn(
        &mut self,
        turn_id: impl Into<String>,
        attribution: TelemetryAttribution,
    ) -> Result<bool, TelemetryError> {
        if !self.config.enabled {
            return Ok(false);
        }
        let turn_id = turn_id.into();
        validate_label("turn_id", &turn_id)?;
        attribution.validate()?;
        if self.active.contains_key(&turn_id)
            || self.completed.iter().any(|record| record.turn_id == turn_id)
        {
            return Err(TelemetryError::DuplicateTurn);
        }
        let record = TurnTelemetry {
            schema_version: OBSERVABILITY_SCHEMA_VERSION,
            turn_id: turn_id.clone(),
            attribution,
            outcome: TelemetryTurnOutcome::Succeeded,
            terminal_state: None,
            waterfall: Vec::new(),
            summary: TelemetrySummary::default(),
            truncated: false,
            dropped_events: 0,
            replay_fixture_candidate: None,
            evidence_artifacts: Vec::new(),
        };
        self.active.insert(turn_id, ActiveTurn { record, next_sequence: 1 });
        Ok(true)
    }

    /// Records start of one privacy-safe waterfall operation.
    pub fn record_started(
        &mut self,
        turn_id: &str,
        elapsed_ms: u64,
        stage: WaterfallStage,
        operation_id: u64,
        tool_name: Option<&str>,
    ) -> Result<(), TelemetryError> {
        self.record_event(turn_id, elapsed_ms, stage, operation_id, true, None, None, tool_name)
    }

    /// Records completion of one waterfall operation. Only tool-execution
    /// failures may include a typed failure reason.
    pub fn record_finished(
        &mut self,
        turn_id: &str,
        finish: WaterfallFinish,
    ) -> Result<(), TelemetryError> {
        if finish.tool_failure.is_some() && finish.stage != WaterfallStage::ToolExecution {
            return Err(TelemetryError::InvalidConfig(
                "only tool executions may carry tool failures",
            ));
        }
        self.record_event(
            turn_id,
            finish.elapsed_ms,
            finish.stage,
            finish.operation_id,
            false,
            Some(finish.outcome),
            finish.tool_failure,
            finish.tool_name.as_deref(),
        )
    }

    /// Completes and retains one turn. Fixture and evidence links require a
    /// failed result so ordinary success records cannot accumulate references.
    pub fn finish_turn(
        &mut self,
        turn_id: &str,
        outcome: TelemetryTurnOutcome,
        summary: TelemetrySummary,
        replay_fixture_candidate: Option<ReplayFixtureCandidate>,
        evidence_artifacts: Vec<RedactedEvidenceRef>,
    ) -> Result<Option<TurnTelemetry>, TelemetryError> {
        self.finish_turn_with_terminal_state(
            turn_id,
            outcome,
            None,
            summary,
            replay_fixture_candidate,
            evidence_artifacts,
        )
    }

    /// Completes a turn while retaining an explicitly host-derived completion
    /// state. `None` means no terminal evidence was available.
    pub fn finish_turn_with_terminal_state(
        &mut self,
        turn_id: &str,
        outcome: TelemetryTurnOutcome,
        terminal_state: Option<CompletionState>,
        summary: TelemetrySummary,
        replay_fixture_candidate: Option<ReplayFixtureCandidate>,
        evidence_artifacts: Vec<RedactedEvidenceRef>,
    ) -> Result<Option<TurnTelemetry>, TelemetryError> {
        if !self.config.enabled {
            return Ok(None);
        }
        summary.validate()?;
        if outcome != TelemetryTurnOutcome::Failed && replay_fixture_candidate.is_some() {
            return Err(TelemetryError::FixtureCandidateRequiresFailure);
        }
        if outcome != TelemetryTurnOutcome::Failed && !evidence_artifacts.is_empty() {
            return Err(TelemetryError::EvidenceArtifactsRequireFailure);
        }
        let Some(mut active) = self.active.remove(turn_id) else {
            return Err(TelemetryError::UnknownTurn);
        };
        let observed_failures = std::mem::take(&mut active.record.summary.tool_failures);
        active.record.outcome = outcome;
        active.record.terminal_state = terminal_state;
        active.record.summary = summary;
        merge_tool_failures(&mut active.record.summary.tool_failures, observed_failures);
        active.record.replay_fixture_candidate = replay_fixture_candidate;
        active.record.evidence_artifacts = evidence_artifacts;
        enforce_byte_cap(&mut active.record, self.config.max_bytes_per_turn)?;
        let record = active.record;
        if self.config.max_turns > 0 {
            self.completed.push_back(record.clone());
            while self.completed.len() > self.config.max_turns {
                self.completed.pop_front();
            }
        }
        Ok(Some(record))
    }

    /// Returns a deterministic snapshot of completed records. All strings were
    /// validated or sanitized at record time; no extra source content exists.
    #[must_use]
    pub fn completed(&self) -> Vec<TurnTelemetry> {
        self.completed.iter().cloned().collect()
    }

    /// Clears active and completed local telemetry immediately.
    pub fn clear(&mut self) {
        self.active.clear();
        self.completed.clear();
    }

    /// Exports completed records as stable local JSONL. This method returns
    /// text only; caller chooses whether and where to write it.
    pub fn export_jsonl(&self) -> Result<String, TelemetryError> {
        let mut output = String::new();
        for record in &self.completed {
            let line = serde_json::to_string(record)
                .map_err(|error| TelemetryError::Serialization(error.to_string()))?;
            output.push_str(&line);
            output.push('\n');
        }
        Ok(output)
    }

    #[allow(clippy::too_many_arguments)]
    fn record_event(
        &mut self,
        turn_id: &str,
        elapsed_ms: u64,
        stage: WaterfallStage,
        operation_id: u64,
        started: bool,
        outcome: Option<WaterfallOutcome>,
        tool_failure: Option<ToolFailureReason>,
        tool_name: Option<&str>,
    ) -> Result<(), TelemetryError> {
        if !self.config.enabled {
            return Ok(());
        }
        let Some(active) = self.active.get_mut(turn_id) else {
            return Err(TelemetryError::UnknownTurn);
        };
        if let Some(reason) = tool_failure {
            active.record.summary.record_tool_failure(reason);
        }
        if active.record.waterfall.len() >= self.config.max_events_per_turn {
            active.record.truncated = true;
            active.record.dropped_events += 1;
            return Ok(());
        }
        let event = WaterfallEvent {
            sequence: active.next_sequence,
            elapsed_ms,
            stage,
            operation_id,
            started,
            outcome,
            tool_failure,
            tool_name: tool_name.map(sanitize_tool_name),
        };
        active.next_sequence += 1;
        active.record.waterfall.push(event);
        self.enforce_byte_cap_for_active(turn_id)
    }

    fn enforce_byte_cap_for_active(&mut self, turn_id: &str) -> Result<(), TelemetryError> {
        let max_bytes_per_turn = self.config.max_bytes_per_turn;
        let Some(active) = self.active.get_mut(turn_id) else {
            return Err(TelemetryError::UnknownTurn);
        };
        enforce_byte_cap(&mut active.record, max_bytes_per_turn)
    }
}

fn enforce_byte_cap(
    record: &mut TurnTelemetry,
    max_bytes_per_turn: usize,
) -> Result<(), TelemetryError> {
    while serialized_len(record)? > max_bytes_per_turn && !record.waterfall.is_empty() {
        record.waterfall.pop();
        record.truncated = true;
        record.dropped_events += 1;
    }
    if serialized_len(record)? > max_bytes_per_turn {
        return Err(TelemetryError::InvalidConfig(
            "max_bytes_per_turn is too small for configured attribution",
        ));
    }
    Ok(())
}

fn validate_config(config: &TelemetryConfig) -> Result<(), TelemetryError> {
    if config.max_events_per_turn == 0 {
        return Err(TelemetryError::InvalidConfig("max_events_per_turn must be non-zero"));
    }
    if config.max_bytes_per_turn < 2_048 {
        return Err(TelemetryError::InvalidConfig("max_bytes_per_turn must be at least 2048"));
    }
    Ok(())
}

fn validate_label(field: &'static str, value: &str) -> Result<(), TelemetryError> {
    if value.is_empty()
        || value.len() > TELEMETRY_LABEL_MAX_CHARS
        || is_secret_like(value)
        || !value.chars().all(|character| {
            character.is_ascii_alphanumeric() || matches!(character, '-' | '_' | '.')
        })
    {
        return Err(TelemetryError::InvalidLabel { field });
    }
    Ok(())
}

fn is_fixture_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= TELEMETRY_LABEL_MAX_CHARS
        && value.chars().all(|character| matches!(character, 'a'..='z' | '0'..='9' | '_'))
}

fn sanitize_tool_name(value: &str) -> String {
    let redacted = redact(value);
    if redacted == "[redacted]" {
        return redacted;
    }
    redacted.chars().take(TELEMETRY_LABEL_MAX_CHARS).collect()
}

fn merge_tool_failures(
    destination: &mut BTreeMap<ToolFailureReason, u64>,
    source: BTreeMap<ToolFailureReason, u64>,
) {
    for (reason, count) in source {
        let entry = destination.entry(reason).or_default();
        *entry = entry.saturating_add(count);
    }
}

fn serialized_len(record: &TurnTelemetry) -> Result<usize, TelemetryError> {
    serde_json::to_vec(record)
        .map(|bytes| bytes.len())
        .map_err(|error| TelemetryError::Serialization(error.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn attribution() -> TelemetryAttribution {
        TelemetryAttribution::new(
            TelemetryVersionLabels {
                provider_version: "provider-v1".into(),
                model_version: "model-v1".into(),
                prompt_version: "prompt-v1".into(),
                manifest_version: "manifest-v1".into(),
                schema_version: "telemetry-v1".into(),
                policy_version: "policy-v1".into(),
                routing_version: "route-v1".into(),
            },
            TelemetryTransport::Acp,
        )
        .expect("valid attribution")
    }

    fn recorder() -> TelemetryRecorder {
        TelemetryRecorder::new(TelemetryConfig {
            enabled: true,
            max_turns: 2,
            max_events_per_turn: 16,
            max_bytes_per_turn: 4_096,
        })
        .expect("valid config")
    }

    #[test]
    fn disabled_recorder_retains_nothing() {
        let mut recorder = TelemetryRecorder::default();
        assert!(!recorder.start_turn("turn_1", attribution()).expect("disabled"));
        recorder
            .record_started("turn_1", 0, WaterfallStage::ModelCall, 1, None)
            .expect("disabled recording is no-op");
        assert_eq!(
            recorder
                .finish_turn(
                    "turn_1",
                    TelemetryTurnOutcome::Succeeded,
                    TelemetrySummary::default(),
                    None,
                    Vec::new(),
                )
                .expect("disabled completion"),
            None
        );
        assert!(recorder.completed().is_empty());
        assert!(recorder.export_jsonl().expect("exports").is_empty());
    }

    #[test]
    fn waterfall_records_every_phase_without_content() {
        let mut recorder = recorder();
        recorder.start_turn("turn_1", attribution()).expect("starts");
        let stages = [
            WaterfallStage::ModelRouting,
            WaterfallStage::ModelCall,
            WaterfallStage::ToolExecution,
            WaterfallStage::Approval,
            WaterfallStage::Retry,
            WaterfallStage::Recovery,
            WaterfallStage::Validation,
        ];
        for (index, stage) in stages.into_iter().enumerate() {
            recorder
                .record_started("turn_1", index as u64, stage, index as u64, Some("read_file"))
                .expect("starts phase");
            recorder
                .record_finished(
                    "turn_1",
                    WaterfallFinish {
                        elapsed_ms: index as u64 + 1,
                        stage,
                        operation_id: index as u64,
                        outcome: WaterfallOutcome::Succeeded,
                        tool_failure: None,
                        tool_name: Some("read_file".into()),
                    },
                )
                .expect("finishes phase");
        }
        let record = recorder
            .finish_turn(
                "turn_1",
                TelemetryTurnOutcome::Succeeded,
                TelemetrySummary {
                    quality_score: Some(100),
                    latency_ms: 12,
                    approval_count: 1,
                    retry_count: 1,
                    repair_count: 1,
                    recovery_count: 1,
                    validation_count: 1,
                    tool_calls: 1,
                    model_calls: 1,
                    estimated_cost_microusd: 4,
                    tool_failures: BTreeMap::new(),
                },
                None,
                Vec::new(),
            )
            .expect("finishes")
            .expect("enabled record");
        assert_eq!(record.waterfall.len(), 14);
        assert_eq!(record.waterfall[0].sequence, 1);
        assert_eq!(record.waterfall[13].sequence, 14);
        assert_eq!(record.waterfall[0].stage, WaterfallStage::ModelRouting);
        assert_eq!(record.waterfall[12].stage, WaterfallStage::Validation);
    }

    #[test]
    fn typed_failures_aggregate_without_error_messages() {
        let mut recorder = recorder();
        recorder.start_turn("turn_1", attribution()).expect("starts");
        recorder
            .record_finished(
                "turn_1",
                WaterfallFinish {
                    elapsed_ms: 4,
                    stage: WaterfallStage::ToolExecution,
                    operation_id: 2,
                    outcome: WaterfallOutcome::Failed,
                    tool_failure: Some(ToolFailureReason::StaleState),
                    tool_name: Some("write_file".into()),
                },
            )
            .expect("records failure");
        let mut summary = TelemetrySummary::default();
        // Explicit host counters merge with typed failures observed directly
        // from waterfall events.
        summary.record_tool_failure(ToolFailureReason::TransportFailure);
        let record = recorder
            .finish_turn(
                "turn_1",
                TelemetryTurnOutcome::Failed,
                summary,
                ReplayFixtureCandidate::new(1, "stale_revision").ok(),
                vec![RedactedEvidenceRef::new("evidence_1").expect("opaque id")],
            )
            .expect("finishes")
            .expect("record");
        assert_eq!(record.summary.tool_failures[&ToolFailureReason::StaleState], 1);
        assert_eq!(record.summary.tool_failures[&ToolFailureReason::TransportFailure], 1);
        let json = serde_json::to_string(&record).expect("serializes");
        assert!(!json.contains("arguments"));
        assert!(!json.contains("error message"));
        assert!(json.contains("stale_state"));
    }

    #[test]
    fn legacy_tool_error_mapping_is_explicit_and_cancellation_is_not_failure() {
        assert_eq!(
            ToolFailureReason::from_tool_error_kind(ToolErrorKind::InvalidArguments),
            Some(ToolFailureReason::InvalidInput)
        );
        assert_eq!(
            ToolFailureReason::from_tool_error_kind(ToolErrorKind::PermissionDenied),
            Some(ToolFailureReason::PolicyDenial)
        );
        assert_eq!(ToolFailureReason::from_tool_error_kind(ToolErrorKind::Cancelled), None);
    }

    #[test]
    fn secret_like_labels_are_rejected_before_retention() {
        let error = TelemetryAttribution::new(
            TelemetryVersionLabels {
                provider_version: "provider-v1".into(),
                model_version: "sk-live-1234567890".into(),
                prompt_version: "prompt-v1".into(),
                manifest_version: "manifest-v1".into(),
                schema_version: "schema-v1".into(),
                policy_version: "policy-v1".into(),
                routing_version: "route-v1".into(),
            },
            TelemetryTransport::Acp,
        )
        .expect_err("secret must fail closed");
        assert_eq!(error, TelemetryError::InvalidLabel { field: "model_version" });

        let mut recorder = recorder();
        recorder.start_turn("turn_1", attribution()).expect("starts");
        recorder
            .record_started(
                "turn_1",
                0,
                WaterfallStage::ToolExecution,
                1,
                Some("OPENROUTER_API_KEY=sk-live-123456"),
            )
            .expect("tool name is sanitized");
        let record = recorder
            .finish_turn(
                "turn_1",
                TelemetryTurnOutcome::Succeeded,
                TelemetrySummary::default(),
                None,
                Vec::new(),
            )
            .expect("finishes")
            .expect("record");
        let json = serde_json::to_string(&record).expect("serializes");
        assert!(json.contains("[redacted]"));
        assert!(!json.contains("sk-live-123456"));
    }

    #[test]
    fn fixture_and_evidence_links_are_failure_only() {
        let mut recorder = recorder();
        recorder.start_turn("turn_1", attribution()).expect("starts");
        let candidate = ReplayFixtureCandidate::new(1, "unsafe_terminal").expect("candidate");
        assert_eq!(
            recorder
                .finish_turn(
                    "turn_1",
                    TelemetryTurnOutcome::Succeeded,
                    TelemetrySummary::default(),
                    Some(candidate),
                    Vec::new(),
                )
                .expect_err("success cannot link a failure fixture"),
            TelemetryError::FixtureCandidateRequiresFailure
        );
        assert!(recorder.active.contains_key("turn_1"), "failed completion preserves active turn");
    }

    #[test]
    fn retention_and_byte_caps_are_deterministic_and_clearable() {
        let mut recorder = TelemetryRecorder::new(TelemetryConfig {
            enabled: true,
            max_turns: 1,
            max_events_per_turn: 16,
            max_bytes_per_turn: 2_048,
        })
        .expect("valid cap");
        for turn in ["turn_1", "turn_2"] {
            recorder.start_turn(turn, attribution()).expect("starts");
            for operation in 0..16 {
                recorder
                    .record_started(
                        turn,
                        operation,
                        WaterfallStage::ToolExecution,
                        operation,
                        Some("a_tool_with_a_deliberately_long_but_bounded_identifier"),
                    )
                    .expect("records");
            }
            recorder
                .finish_turn(
                    turn,
                    TelemetryTurnOutcome::Succeeded,
                    TelemetrySummary::default(),
                    None,
                    Vec::new(),
                )
                .expect("finishes");
        }
        let completed = recorder.completed();
        assert_eq!(completed.len(), 1);
        assert_eq!(completed[0].turn_id, "turn_2");
        assert!(completed[0].truncated);
        assert!(completed[0].dropped_events > 0);
        let jsonl = recorder.export_jsonl().expect("exports");
        assert_eq!(jsonl.lines().count(), 1);
        assert!(jsonl.len() <= 2_048, "retained record must honor byte cap");
        recorder.clear();
        assert!(recorder.completed().is_empty());
        assert!(recorder.export_jsonl().expect("exports").is_empty());
    }

    #[test]
    fn invalid_retention_configs_fail_closed() {
        assert!(matches!(
            TelemetryRecorder::new(TelemetryConfig {
                enabled: true,
                max_turns: 1,
                max_events_per_turn: 0,
                max_bytes_per_turn: 2_048,
            }),
            Err(TelemetryError::InvalidConfig(_))
        ));
        assert!(matches!(
            TelemetryRecorder::new(TelemetryConfig {
                enabled: true,
                max_turns: 1,
                max_events_per_turn: 1,
                max_bytes_per_turn: 2_047,
            }),
            Err(TelemetryError::InvalidConfig(_))
        ));
    }
}
